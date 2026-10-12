use actix_web::{ web, Scope, HttpResponse };
use serde::{ Deserialize, Serialize };
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::sql_types::{ BigInt, Bool, Double, Nullable, Text, Uuid as SqlUuid };
use uuid::Uuid;
use chrono::Utc;
use crate::db::DbPool;
use crate::api::errors::{ ApiError, blocking };
use crate::api::riders::{ CreateRideRequest, OrderSource, delivery_status, db_now_nullable };
use crate::api::drivers::refresh_driver_status;
use crate::services::auth::AuthUser;
use crate::services::pricing::GeoPoint;
use crate::services::{ codes, korapay, zazu };

// The parts of app_delivery_orders the backend decides on. status is read as text and the
// numeric coordinates as float8, so no bigdecimal is needed.
#[derive(QueryableByName, Debug, Clone)]
pub struct OrderRow {
    #[diesel(sql_type = BigInt)]
    pub id: i64,
    #[diesel(sql_type = SqlUuid)]
    pub client_id: Uuid,
    #[diesel(sql_type = Text)]
    pub status: String,
    #[diesel(sql_type = Text)]
    pub source: String,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    pub driver_id: Option<Uuid>,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    pub order_ref: Option<Uuid>,
    #[diesel(sql_type = Nullable<Text>)]
    pub payment_reference: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub pickup_code: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub dropoff_code: Option<String>,
    #[diesel(sql_type = Bool)]
    pub is_pickup_code_authenticated: bool,
    #[diesel(sql_type = Bool)]
    pub is_dropoff_code_authenticated: bool,
    #[diesel(sql_type = Nullable<Double>)]
    pub pickup_lat: Option<f64>,
    #[diesel(sql_type = Nullable<Double>)]
    pub pickup_long: Option<f64>,
    #[diesel(sql_type = Nullable<Double>)]
    pub dropoff_lat: Option<f64>,
    #[diesel(sql_type = Nullable<Double>)]
    pub dropoff_long: Option<f64>,
}

impl OrderRow {
    pub fn source(&self) -> OrderSource {
        if self.source.eq_ignore_ascii_case("zazu") { OrderSource::Zazu } else { OrderSource::App }
    }

    pub fn pick_up(&self) -> Option<GeoPoint> {
        Some(GeoPoint { lat: self.pickup_lat?, lng: self.pickup_long?, name: None })
    }

    pub fn drop_off(&self) -> Option<GeoPoint> {
        Some(GeoPoint { lat: self.dropoff_lat?, lng: self.dropoff_long?, name: None })
    }
}

pub enum OrderKey<'a> {
    Id(i64),
    Ref(Uuid),
    // The short code shown in the apps; they send it in the order_ref field.
    Code(&'a str),
    PaymentReference(&'a str),
}

impl<'a> OrderKey<'a> {
    // Accepts either the order_ref UUID or the order_code.
    pub fn parse(value: &'a str) -> Self {
        let value = value.trim();
        match Uuid::parse_str(value) {
            Ok(id) => OrderKey::Ref(id),
            Err(_) => OrderKey::Code(value),
        }
    }
}

const ORDER_COLUMNS: &str = "id, client_id, status::text AS status, source, driver_id, order_ref, payment_reference, \
    pickup_code, dropoff_code, is_pickup_code_authenticated, is_dropoff_code_authenticated, \
    pickup_lat::float8 AS pickup_lat, pickup_long::float8 AS pickup_long, \
    dropoff_lat::float8 AS dropoff_lat, dropoff_long::float8 AS dropoff_long";

// Loads one order; with `lock` the row stays locked until the caller's transaction ends.
pub fn load_order(conn: &mut PgConnection, key: OrderKey, lock: bool) -> QueryResult<Option<OrderRow>> {
    let lock = if lock { " FOR UPDATE" } else { "" };
    let query = |filter: &str| format!("SELECT {} FROM app_delivery_orders WHERE {} LIMIT 1{}", ORDER_COLUMNS, filter, lock);

    match key {
        OrderKey::Id(id) => diesel::sql_query(query("id = $1"))
            .bind::<BigInt, _>(id)
            .get_result(conn),
        OrderKey::Ref(order_ref) => diesel::sql_query(query("order_ref = $1"))
            .bind::<SqlUuid, _>(order_ref)
            .get_result(conn),
        OrderKey::Code(code) => diesel::sql_query(query("order_code = $1"))
            .bind::<Text, _>(code)
            .get_result(conn),
        OrderKey::PaymentReference(reference) => diesel::sql_query(query("payment_reference = $1"))
            .bind::<Text, _>(reference)
            .get_result(conn),
    }
    .optional()
}

pub struct NewOrder<'a> {
    pub client_id: Uuid,
    // Fixed literals only: awaiting_payment (app) or pending (Zazu).
    pub status: &'static str,
    pub source: OrderSource,
    pub order_ref: Uuid,
    pub payment_reference: Option<&'a str>,
    pub customer_phone: &'a str,
    pub request: &'a CreateRideRequest,
}

#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

fn package_description(request: &CreateRideRequest) -> Option<String> {
    let parts: Vec<String> = request.items.iter()
        .map(|item| format!("{} x {}", item.quantity, item.name.trim()))
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

// Inserts the delivery order with a fresh order_code and its pickup and drop-off codes. Every
// order gets codes, Zazu's too: the driver app enters them here and this backend checks them.
// Zazu receives them with the ride details once a driver takes the order.
pub fn insert_order(conn: &mut PgConnection, order: &NewOrder) -> QueryResult<i64> {
    let req = order.request;
    let pickup_name = req.pickup_name.clone().or_else(|| req.pick_up.name.clone());
    let dropoff_name = req.dropoff_name.clone().or_else(|| req.drop_off.name.clone());
    let image_url = req.items.iter().find_map(|item| item.image_url.clone());
    let description = package_description(req);
    let (pickup_code, dropoff_code) = codes::code_pair();

    // order_code is unique; a clash (rare: 31^8 codes) just draws another.
    for _ in 0..5 {
        let inserted = diesel::sql_query(
            "INSERT INTO app_delivery_orders (client_id, status, source, order_ref, \
                pickup_lat, pickup_long, dropoff_lat, dropoff_long, pickup_name, dropoff_name, \
                customer_phone, pickup_contact_name, pickup_contact_phone, payment_reference, \
                image_url, package_description, order_code, pickup_code, dropoff_code) \
             VALUES ($1, CAST($2 AS delivery_status), $3, $4, \
                CAST($5 AS numeric), CAST($6 AS numeric), CAST($7 AS numeric), CAST($8 AS numeric), $9, $10, \
                $11, $12, $13, $14, $15, $16, $17, $18, $19) \
             ON CONFLICT (order_code) DO NOTHING \
             RETURNING id"
        )
        .bind::<SqlUuid, _>(order.client_id)
        .bind::<Text, _>(order.status)
        .bind::<Text, _>(order.source.as_str())
        .bind::<SqlUuid, _>(order.order_ref)
        .bind::<Double, _>(req.pick_up.lat)
        .bind::<Double, _>(req.pick_up.lng)
        .bind::<Double, _>(req.drop_off.lat)
        .bind::<Double, _>(req.drop_off.lng)
        .bind::<Nullable<Text>, _>(pickup_name.as_deref())
        .bind::<Nullable<Text>, _>(dropoff_name.as_deref())
        .bind::<Text, _>(order.customer_phone)
        .bind::<Nullable<Text>, _>(req.pickup_contact_name.as_deref())
        .bind::<Nullable<Text>, _>(req.pickup_contact_phone.as_deref())
        .bind::<Nullable<Text>, _>(order.payment_reference)
        .bind::<Nullable<Text>, _>(image_url.as_deref())
        .bind::<Nullable<Text>, _>(description.as_deref())
        .bind::<Text, _>(codes::order_code())
        .bind::<Text, _>(&pickup_code)
        .bind::<Text, _>(&dropoff_code)
        .get_result::<IdRow>(conn)
        .optional()?;

        if let Some(row) = inserted {
            return Ok(row.id);
        }
    }
    Err(diesel::result::Error::QueryBuilderError("could not generate a unique order_code".into()))
}

// Gives an order created before every order had codes (older Zazu orders) its pair. Never
// touches codes that are already set.
pub fn ensure_codes(conn: &mut PgConnection, order_id: i64) -> QueryResult<()> {
    let (pickup_code, dropoff_code) = codes::code_pair();
    diesel::sql_query(
        "UPDATE app_delivery_orders SET pickup_code = $2, dropoff_code = $3 \
         WHERE id = $1 AND pickup_code IS NULL AND dropoff_code IS NULL"
    )
    .bind::<BigInt, _>(order_id)
    .bind::<Text, _>(&pickup_code)
    .bind::<Text, _>(&dropoff_code)
    .execute(conn)?;
    Ok(())
}

// The trip record of an order (back_trips), keyed by order_ref, app and Zazu alike:
//   Ongoing         pickup confirmed, package on the way
//   AwaitingPayout  app order delivered; the driver's payout is sent, Korapay hasn't confirmed it
//   Completed       Zazu: at drop-off. App: once Korapay confirms the payout (end_ts set)
//   PayoutFailed    Korapay reports the app driver's payout failed; needs someone to look at it
//   Cancelled       the order was cancelled after pickup

// A Solana pubkey as text. Accounts still carry placeholders ("pubkey_...", {}); those are stored
// as NULL.
const BASE58_PUBKEY: &str = "^[1-9A-HJ-NP-Za-km-z]{32,44}$";

// Opens the trip when the pickup is confirmed, built from the order, the rider and the driver.
// trip_id is derived from order_ref, so a repeat inserts nothing.
pub fn open_trip(conn: &mut PgConnection, order_id: i64) -> QueryResult<()> {
    let opened = diesel::sql_query(
        "INSERT INTO back_trips (trip_id, rider_id, reference, pick_up, drop_off, driver_location, \
            rider_pubkey, driver_pubkey, driver_id, status, start_ts, end_ts, distance_km, item, \
            fare_estimate, fare_lamports, order_ref, rider_email) \
         SELECT sha256(convert_to(o.order_ref::text, 'UTF8')), o.client_id, \
            COALESCE(o.payment_reference, o.order_code), \
            jsonb_build_object('lat', o.pickup_lat::float8, 'lng', o.pickup_long::float8, 'name', o.pickup_name), \
            jsonb_build_object('lat', o.dropoff_lat::float8, 'lng', o.dropoff_long::float8, 'name', o.dropoff_name), \
            d.driver_location, \
            CASE WHEN r.rider_pubkey #>> '{}' ~ $2 THEN r.rider_pubkey #>> '{}' END, \
            CASE WHEN d.driver_pubkey #>> '{}' ~ $2 THEN d.driver_pubkey #>> '{}' END, \
            d.driver_id, 'Ongoing', \
            extract(epoch from now())::int8, NULL, COALESCE(rr.distance_km, 0), COALESCE(rr.items, '[]'::jsonb), \
            COALESCE(round(o.price)::int8, rr.estimated_price), NULL, o.order_ref, r.email \
         FROM app_delivery_orders o \
         JOIN back_custom_users r ON r.rider_id = o.client_id \
         JOIN back_drivers d ON d.driver_id = o.driver_id \
         LEFT JOIN LATERAL (SELECT distance_km, items, estimated_price FROM back_ride_request \
            WHERE order_ref = o.order_ref LIMIT 1) rr ON true \
         WHERE o.id = $1 AND o.order_ref IS NOT NULL \
         ON CONFLICT (trip_id) DO NOTHING"
    )
    .bind::<BigInt, _>(order_id)
    .bind::<Text, _>(BASE58_PUBKEY)
    .execute(conn)?;
    if opened == 0 {
        // Already open (a repeated pickup), or no rider account / order_ref to build it from.
        eprintln!("Order {}: no new trip record opened at pickup", order_id);
    }
    Ok(())
}

// App order delivered: the trip waits on the driver's payout before it counts as Completed.
pub fn await_payout(conn: &mut PgConnection, order_ref: Option<Uuid>) -> QueryResult<()> {
    use crate::schema::back_trips::dsl as t;

    let Some(order_ref) = order_ref else { return Ok(()) };
    diesel::update(t::back_trips.filter(t::order_ref.eq(order_ref)).filter(t::status.eq("Ongoing")))
        .set(t::status.eq("AwaitingPayout"))
        .execute(conn)?;
    Ok(())
}

pub fn payout_failed(conn: &mut PgConnection, order_ref: Option<Uuid>) -> QueryResult<()> {
    use crate::schema::back_trips::dsl as t;

    let Some(order_ref) = order_ref else { return Ok(()) };
    diesel::update(t::back_trips.filter(t::order_ref.eq(order_ref)).filter(t::status.eq("AwaitingPayout")))
        .set(t::status.eq("PayoutFailed"))
        .execute(conn)?;
    Ok(())
}

// Ends the trip record that mirrors the order, if one exists. Returns whether this call ended it.
pub fn close_trip(conn: &mut PgConnection, order_ref: Option<Uuid>, status: &'static str) -> QueryResult<bool> {
    use crate::schema::back_trips::dsl as t;

    let Some(order_ref) = order_ref else { return Ok(false) };
    let updated = diesel::update(
        t::back_trips
            .filter(t::order_ref.eq(order_ref))
            .filter(t::status.ne_all(vec!["Completed", "Cancelled"]))
    )
    .set((
        t::status.eq(status),
        t::end_ts.eq(Some(Utc::now().timestamp())),
    ))
    .execute(conn)?;
    Ok(updated > 0)
}

// Lapses any offer still waiting on a driver, including an assigned delivery nobody started.
pub fn expire_open_offers(conn: &mut PgConnection, order_id: i64) -> QueryResult<()> {
    use crate::schema::app_ride_offers::dsl as ro;

    diesel::update(
        ro::app_ride_offers
            .filter(ro::order_id.eq(order_id))
            .filter(
                ro::status.eq("pending")
                    .or(ro::status.eq("accepted").and(ro::responded_at.is_null()))
            )
    )
    .set(ro::status.eq("expired"))
    .execute(conn)?;
    Ok(())
}

// Unpaid order abandoned at checkout. Only moves awaiting_payment, so a payment that landed
// first wins. Returns whether this call cancelled it.
pub fn cancel_unpaid(conn: &mut PgConnection, order_id: i64, cancelled_by: Option<Uuid>) -> QueryResult<bool> {
    use crate::schema::app_delivery_orders::dsl as o;

    let updated = diesel::update(
        o::app_delivery_orders
            .filter(o::id.eq(order_id))
            .filter(o::status.eq(delivery_status("awaiting_payment")))
    )
    .set((
        o::status.eq(delivery_status("cancelled")),
        o::cancelled_at.eq(db_now_nullable()),
        o::cancelled_by.eq(cancelled_by),
        o::modified_at.eq(diesel::dsl::now),
    ))
    .execute(conn)?;
    Ok(updated == 1)
}

// What Korapay actually charged for this reference, in whole naira; None if it wasn't paid.
// Refunds use this rather than a stored quote: the checkout amount was fixed server-side.
pub async fn paid_amount(payment_reference: &str) -> Result<Option<i64>, String> {
    match korapay::query_charge(payment_reference).await? {
        korapay::ChargeStatus::Success { amount, .. } => Ok(Some(amount.floor() as i64)),
        _ => Ok(None),
    }
}

// Either key identifies the order; order_ref wins when both are sent.
#[derive(Deserialize)]
pub struct CancelOrderRequest {
    #[serde(default)]
    pub order_ref: Option<Uuid>,
    #[serde(default)]
    pub payment_reference: Option<String>,
}

#[derive(Serialize)]
pub struct CancelOrderResponse {
    pub order_id: i64,
    pub status: &'static str,
    // Naira going back to the customer; 0 once a rider was assigned.
    pub refund_amount: i64,
    // none, initiated, or failed (safe to retry the call).
    pub refund_status: &'static str,
    // The rider is taking the package back to the pickup.
    pub returning_to_pickup: bool,
}

struct Cancelled {
    order_id: i64,
    payment_reference: Option<String>,
    // No rider was assigned, so whatever was paid goes back.
    refund_due: bool,
    returning: bool,
}

// Customer taps "Cancel & refund". Refund rules:
//   no rider yet          -> full refund of what Korapay charged (also before the ride request)
//   rider assigned        -> no refund
//   pickup code entered   -> no refund, and the rider takes the package back (app_returned)
// Order, offers, trip and the driver's status change in one transaction; the refund is sent
// after it commits. Retrying a cancelled order re-sends the refund under the same reference,
// which Korapay won't pay out twice.
pub async fn cancel_order(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<CancelOrderRequest>,
) -> HttpResponse {
    use crate::schema::app_delivery_orders::dsl as o;

    let me = auth.user_id;
    let req = body.into_inner();
    let reference = req.payment_reference.as_deref().map(str::trim).filter(|r| !r.is_empty()).map(str::to_string);
    let order_ref = req.order_ref;
    if order_ref.is_none() && reference.is_none() {
        return ApiError::BadRequest("order_ref or payment_reference is required".into()).response();
    }

    let find = move |conn: &mut PgConnection, lock: bool| -> Result<OrderRow, ApiError> {
        let key = match (order_ref, reference.as_deref()) {
            (Some(order_ref), _) => OrderKey::Ref(order_ref),
            (None, Some(reference)) => OrderKey::PaymentReference(reference),
            (None, None) => unreachable!("checked above"),
        };
        load_order(conn, key, lock)?
            .filter(|order| order.client_id == me)
            .ok_or_else(|| ApiError::NotFound("No order found".into()))
    };

    // Before payment the checkout may still be open. A charge Korapay is still processing could
    // land after the cancel with nothing left to refund it, so that has to settle first.
    let order = match blocking(&pool, { let find = find.clone(); move |conn| find(conn, false) }).await {
        Ok(order) => order,
        Err(e) => return e.response(),
    };
    if order.status == "awaiting_payment" {
        if let Some(reference) = order.payment_reference.as_deref() {
            match korapay::query_charge(reference).await {
                Ok(korapay::ChargeStatus::Processing) =>
                    return ApiError::Conflict("The payment is still processing; try again in a moment".into()).response(),
                Ok(_) => {},
                Err(e) => return ApiError::BadGateway(format!("Could not check the payment: {}", e)).response(),
            }
        }
    }

    let result = blocking(&pool, move |conn| conn.transaction(|conn| {
            let order = find(conn, true)?;
            let unassigned = order.driver_id.is_none();
            let cancelled = |returning| Cancelled {
                order_id: order.id,
                payment_reference: order.payment_reference.clone(),
                refund_due: unassigned,
                returning,
            };

            match order.status.as_str() {
                "delivered" => return Err(ApiError::Conflict("Order was already delivered".into())),
                "cancelled" => return Ok(cancelled(order.is_pickup_code_authenticated && order.source() == OrderSource::App)),
                "awaiting_payment" => {
                    // Paid but no ride request yet: refunded below like any unassigned order.
                    cancel_unpaid(conn, order.id, Some(me))?;
                    return Ok(cancelled(false));
                }
                _ => {}
            }

            diesel::update(o::app_delivery_orders.filter(o::id.eq(order.id)))
                .set((
                    o::status.eq(delivery_status("cancelled")),
                    o::cancelled_at.eq(db_now_nullable()),
                    o::cancelled_by.eq(Some(me)),
                    o::modified_at.eq(diesel::dsl::now),
                ))
                .execute(conn)?;

            expire_open_offers(conn, order.id)?;
            close_trip(conn, order.order_ref, "Cancelled")?;

            // Zazu orders have no returns.
            let returning = order.is_pickup_code_authenticated && order.source() == OrderSource::App;
            if returning {
                open_return(conn, &order)?;
            }
            if let Some(driver) = order.driver_id {
                // A returning rider still counts as carrying it (see drivers::active_order_count).
                refresh_driver_status(conn, driver)?;
            }

            Ok(cancelled(returning))
    })).await;

    let cancelled = match result {
        Ok(c) => c,
        Err(e) => return e.response(),
    };
    zazu::spawn_notify_status(pool.clone(), cancelled.order_id);

    let reference = match (cancelled.refund_due, cancelled.payment_reference.as_deref()) {
        (true, Some(reference)) => Some(reference),
        _ => None,
    };
    let refund_amount = match reference {
        Some(reference) => match paid_amount(reference).await {
            Ok(amount) => amount.unwrap_or(0),
            Err(e) => {
                // The order is cancelled; retrying this call re-checks and refunds.
                eprintln!("Order {}: could not check payment for refund: {}", cancelled.order_id, e);
                return ApiError::BadGateway(format!("Order cancelled, but the payment could not be checked for a refund; retry: {}", e)).response();
            }
        },
        None => 0,
    };

    let refund_status = match reference {
        Some(reference) if refund_amount > 0 => match korapay::refund(reference, refund_amount, "Order cancelled before a rider was assigned").await {
            Ok(()) => "initiated",
            Err(e) => {
                eprintln!("Order {}: refund of {} failed: {}", cancelled.order_id, refund_amount, e);
                "failed"
            }
        },
        _ => "none",
    };

    HttpResponse::Ok().json(CancelOrderResponse {
        order_id: cancelled.order_id,
        status: "cancelled",
        refund_amount,
        refund_status,
        returning_to_pickup: cancelled.returning,
    })
}

// The package goes back to the pickup point. The return code is generated like the pickup and
// drop-off codes, and never equals either of them; the client reads it through
// get_return_code() and gives it to the rider.
fn open_return(conn: &mut PgConnection, order: &OrderRow) -> QueryResult<()> {
    let taken: Vec<&str> = [order.pickup_code.as_deref(), order.dropoff_code.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    diesel::sql_query(
        "INSERT INTO app_returned (order_id, client_id, driver_id, return_lat, return_long, return_name, return_code) \
         SELECT id, client_id, driver_id, pickup_lat, pickup_long, pickup_name, $2 \
         FROM app_delivery_orders WHERE id = $1 \
         ON CONFLICT (order_id) WHERE status IN ('returning', 'returned') DO NOTHING"
    )
    .bind::<BigInt, _>(order.id)
    .bind::<Text, _>(codes::confirmation_code_except(&taken))
    .execute(conn)?;
    Ok(())
}

pub fn routes() -> Scope {
    web::scope("/orders")
        .route("/cancel", web::post().to(cancel_order))
}
