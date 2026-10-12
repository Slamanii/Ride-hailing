use actix_web::{ web, Scope, HttpResponse, http::StatusCode };
use serde::{ Serialize, Deserialize };
use diesel::prelude::*;
use diesel::sql_types::{ Array, BigInt, Double, Nullable, Text, Uuid as SqlUuid };
use uuid::Uuid;
use crate::db::{ DbPool };
use diesel::pg::PgConnection;
use crate::api::errors::{ ApiError, blocking, json_error, message_body };
use crate::api::orders::{ OrderKey, OrderRow, await_payout, close_trip, load_order, open_trip };
use crate::api::riders::{ RideType, OrderSource, DRIVER_RESPONSES, delivery_status, db_now_nullable, same_place };
use crate::services::pricing::{ GeoPoint, distance_between, minimum_distance_between_driver_and_pickup };
use crate::services::auth::{ AuthUser, Caller };
use crate::services::korapay::{ self, CreditStatus, DriverCredit };
use crate::services::{ escrow, zazu };
use tokio::sync::Mutex as TokioMutex;
use tokio::time::sleep;
use std::time::Duration;
use std::env;
use lazy_static::lazy_static;
use chrono::{ DateTime, Utc };

lazy_static! {
    pub static ref ZAZU_URL: TokioMutex<String> = TokioMutex::new(
        env::var("ZAZU_URL").unwrap_or_default()
    );
}


// Always the signed-in driver; a body driver_id for anyone else is rejected.
pub async fn update_driver(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<Driver>,
) -> HttpResponse {
    use crate::schema::back_drivers::dsl::{back_drivers as drivers, *};

    let me = auth.user_id;
    let driver = body.into_inner();
    if driver.driver_id != me {
        return json_error(StatusCode::FORBIDDEN, "driver_id does not match the signed-in driver");
    }
    let result = blocking(&pool, move |conn| {
        Ok(diesel::update(drivers.filter(driver_id.eq(me)))
            .set((
                DriverUpdate {
                    driver_location: driver.driver_location,
                    driver_response: driver.driver_response,
                },
                location_updated_at.eq(db_now_nullable()),
            ))
            .execute(conn)?)
    }).await;

    match result {
        Ok(rows) if rows > 0 => HttpResponse::Ok().json(rows),
        Ok(_) => json_error(StatusCode::NOT_FOUND, "Driver not found"),
        Err(e) => e.response(),
    }
}


pub fn verify_driver_account(connection: &mut PgConnection, driver_id: Uuid) -> Result<Driver, String> {
    use crate::schema::back_drivers::dsl as d;

    let driver: Driver = d::back_drivers
        .find(driver_id)
        .select(Driver::as_select())
        .first::<Driver>(connection)
        .map_err(|e| format!("Driver not found: {}", e))?;

    // busy only means every slot is taken; a busy driver can still finish trips.
    if driver.status == "offline" {
        return Err("Driver account is offline".into());
    }

    Ok(driver)
}

#[derive(QueryableByName)]
struct ReturningCount {
    #[diesel(sql_type = SqlUuid)]
    driver_id: Uuid,
    #[diesel(sql_type = BigInt)]
    count: i64,
}

// Packages each driver is taking back to the pickup after a cancel (app_returned, status
// returning). app_returned belongs to ASAP_APPS and isn't in schema.rs, hence raw SQL.
pub fn returning_counts(connection: &mut PgConnection, driver_ids: &[Uuid]) -> QueryResult<Vec<(Uuid, i64)>> {
    if driver_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<ReturningCount> = diesel::sql_query(
        "SELECT driver_id, count(*) AS count FROM app_returned \
         WHERE status = 'returning' AND driver_id = ANY($1) \
         GROUP BY driver_id"
    )
    .bind::<Array<SqlUuid>, _>(driver_ids)
    .load(connection)?;
    Ok(rows.into_iter().map(|row| (row.driver_id, row.count)).collect())
}

// Orders a driver is carrying right now: assigned and not yet delivered, plus packages they
// are taking back to the pickup.
pub fn active_order_count(connection: &mut PgConnection, driver_uuid: Uuid) -> QueryResult<i64> {
    use crate::schema::app_delivery_orders::dsl as o;

    let carrying: i64 = o::app_delivery_orders
        .filter(o::driver_id.eq(driver_uuid))
        .filter(
            o::status.eq(delivery_status("arriving_pickup"))
                .or(o::status.eq(delivery_status("driver_arrived")))
                .or(o::status.eq(delivery_status("in_transit")))
        )
        .count()
        .get_result(connection)?;
    let returning: i64 = returning_counts(connection, &[driver_uuid])?
        .into_iter()
        .map(|(_, count)| count)
        .sum();
    Ok(carrying + returning)
}

// Bikes can stack two deliveries; every other vehicle carries one at a time.
pub fn max_active_orders(vehicle_type: &str) -> i64 {
    if vehicle_type == "Bike" { 2 } else { 1 }
}

// Moves the driver between available and busy by their load: busy once every slot is taken,
// available while one is free, so an en-route bike can still be offered a second order.
// The online toggle is update_driver_status: this only ever touches a driver whose status
// is already available or busy, so a driver who switched offline (toggle, device or location
// permission) is never switched back on from here.
pub fn refresh_driver_status(connection: &mut PgConnection, driver_uuid: Uuid) -> QueryResult<()> {
    use crate::schema::back_drivers::dsl as d;

    let vehicle: String = d::back_drivers
        .find(driver_uuid)
        .select(d::vehicle_type)
        .first(connection)?;
    let active = active_order_count(connection, driver_uuid)?;
    let next = if active >= max_active_orders(&vehicle) { "busy" } else { "available" };

    diesel::update(
        d::back_drivers
            .filter(d::driver_id.eq(driver_uuid))
            .filter(d::status.eq_any(vec!["available", "busy"]))
    )
    .set(d::status.eq(next))
    .execute(connection)?;
    Ok(())
}

#[derive(Deserialize)]
pub struct DriverStatusInput {
    pub status: String,
}

// The driver app's online toggle. Only available and offline can be asked for: busy belongs
// to refresh_driver_status, so a busy driver can't clear it here, and a driver still carrying
// an order can't go offline (nothing would ever bring their status back).
pub async fn update_driver_status(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<DriverStatusInput>,
) -> HttpResponse {
    let me = auth.user_id;
    let wanted = body.into_inner().status;
    if wanted != "available" && wanted != "offline" {
        return json_error(StatusCode::BAD_REQUEST, "status must be available or offline");
    }

    let result = blocking(&pool, move |conn| {
        conn.transaction::<_, ApiError, _>(|conn| {
            use crate::schema::back_drivers::dsl as d;

            // Locked so an order can't be assigned between the checks and the update.
            let current: String = d::back_drivers
                .find(me)
                .select(d::status)
                .for_update()
                .first(conn)
                .optional()?
                .ok_or_else(|| ApiError::NotFound("Driver not found".into()))?;

            if current == "busy" {
                return Err(ApiError::Conflict("Driver is busy with an order".into()));
            }
            if wanted == "offline" && active_order_count(conn, me)? > 0 {
                return Err(ApiError::Conflict("Finish your active orders before going offline".into()));
            }

            diesel::update(
                d::back_drivers
                    .filter(d::driver_id.eq(me))
                    .filter(d::status.ne("busy"))
            )
            .set(d::status.eq(&wanted))
            .execute(conn)?;

            // Back online with a load already on (a bike carrying one order): settle available/busy.
            if wanted == "available" {
                refresh_driver_status(conn, me)?;
            }

            Ok(d::back_drivers.find(me).select(d::status).first::<String>(conn)?)
        })
    }).await;

    match result {
        Ok(status) => HttpResponse::Ok().json(serde_json::json!({ "status": status })),
        Err(e) => e.response(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AssignOutcome {
    Assigned,
    // Another driver has it, or it's no longer pending (cancelled, ...).
    OrderTaken,
    // Offline, or every slot is already taken.
    DriverUnavailable,
}

// Gives the order to the driver. Run it inside the caller's transaction: it changes nothing
// unless it returns Assigned. Locks the driver row so two orders can't both take their last
// slot, and so the driver can't go offline halfway through.
pub fn assign_order(connection: &mut PgConnection, order_id: i64, driver_uuid: Uuid) -> QueryResult<AssignOutcome> {
    use crate::schema::back_drivers::dsl as d;
    use crate::schema::app_delivery_orders::dsl as o;
    use crate::schema::app_ride_offers::dsl as ro;

    let (driver_status, vehicle) = d::back_drivers
        .find(driver_uuid)
        .select((d::status, d::vehicle_type))
        .for_update()
        .first::<(String, String)>(connection)?;

    if driver_status == "offline"
        || active_order_count(connection, driver_uuid)? >= max_active_orders(&vehicle)
    {
        return Ok(AssignOutcome::DriverUnavailable);
    }

    let taken = diesel::update(
        o::app_delivery_orders
            .filter(o::id.eq(order_id))
            .filter(o::driver_id.is_null())
            .filter(o::status.eq(delivery_status("pending")))
    )
    .set((
        o::driver_id.eq(Some(driver_uuid)),
        o::status.eq(delivery_status("arriving_pickup")),
        o::delivery_accepted_time.eq(db_now_nullable()),
        o::modified_at.eq(diesel::dsl::now),
    ))
    .execute(connection)?;

    if taken == 0 {
        return Ok(AssignOutcome::OrderTaken);
    }

    // The winning offer is already accepted, so this only clears stragglers.
    diesel::update(
        ro::app_ride_offers
            .filter(ro::order_id.eq(order_id))
            .filter(ro::status.eq("pending"))
    )
    .set(ro::status.eq("expired"))
    .execute(connection)?;

    refresh_driver_status(connection, driver_uuid)?;
    Ok(AssignOutcome::Assigned)
}

fn answer_offer(connection: &mut PgConnection, me: Uuid, offer_id: Uuid, accepted: bool) -> Result<OfferAnswer, ApiError> {
    use crate::schema::app_ride_offers::dsl as ro;

    let offer = ro::app_ride_offers
        .find(offer_id)
        .select(RideOffer::as_select())
        .first::<RideOffer>(connection)
        .optional()?
        .filter(|offer| offer.driver_id == me)
        .ok_or_else(|| ApiError::NotFound("Offer not found".into()))?;

    connection.transaction(|conn| {
        // Assigned delivery: already theirs, the only answer is Start.
        if offer.status == "accepted" && offer.responded_at.is_none() {
            if !accepted {
                return Err(ApiError::Conflict("Assigned deliveries can't be declined".into()));
            }
            let started = diesel::update(
                ro::app_ride_offers
                    .filter(ro::id.eq(offer_id))
                    .filter(ro::driver_id.eq(me))
                    .filter(ro::status.eq("accepted"))
                    .filter(ro::responded_at.is_null())
                    .filter(ro::expires_at.gt(diesel::dsl::now))
            )
            .set(ro::responded_at.eq(db_now_nullable()))
            .execute(conn)?;

            if started == 0 {
                return Err(ApiError::Conflict("Delivery is no longer assigned to you".into()));
            }
            return Ok(OfferAnswer { offer_id, order_id: offer.order_id, status: "started" });
        }

        if offer.status != "pending" {
            return Err(ApiError::Conflict("Offer expired or already answered".into()));
        }

        let new_status = if accepted { "accepted" } else { "rejected" };
        let updated = diesel::update(
            ro::app_ride_offers
                .filter(ro::id.eq(offer_id))
                .filter(ro::driver_id.eq(me))
                .filter(ro::status.eq("pending"))
                .filter(ro::expires_at.gt(diesel::dsl::now))
        )
        .set((
            ro::status.eq(new_status),
            ro::responded_at.eq(db_now_nullable()),
        ))
        .execute(conn)?;

        if updated == 0 {
            return Err(ApiError::Conflict("Offer expired or already answered".into()));
        }

        if accepted {
            match assign_order(conn, offer.order_id, me)? {
                AssignOutcome::Assigned => {},
                AssignOutcome::OrderTaken => return Err(ApiError::Conflict("Order is no longer available".into())),
                AssignOutcome::DriverUnavailable => return Err(ApiError::Conflict("You are offline or at your delivery limit".into())),
            }
        }

        Ok(OfferAnswer { offer_id, order_id: offer.order_id, status: new_status })
    })
}

// Driver app answers an offer, or taps Start on an assigned delivery. The driver is whoever
// the token belongs to, never the body. An accept assigns the order in the same transaction,
// so a 200 means the order is theirs. Once a driver has taken a Zazu order (accept, or Start
// on an assigned one), Zazu is sent the ride details.
pub async fn driver_response_handler(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    payload: web::Json<DriverResponsePayload>,
) -> HttpResponse {
    let me = auth.user_id;
    let DriverResponsePayload { offer_id, accepted } = payload.into_inner();

    match blocking(&pool, move |conn| answer_offer(conn, me, offer_id, accepted)).await {
        Ok(answer) => {
            // Wake the search early; if nobody is waiting, the row already says it all.
            if let Some(tx) = DRIVER_RESPONSES.lock().await.remove(&me) {
                let _ = tx.send(());
            }
            if answer.status == "accepted" || answer.status == "started" {
                tokio::spawn(forward_ride_details_to_zazu(pool.clone(), answer.order_id, me));
            }
            HttpResponse::Ok().json(answer)
        }
        Err(e) => e.response(),
    }
}

pub async fn preflight_check(
    caller: Caller,
    pool: web::Data<DbPool>,
    req: web::Json<RidePreflightRequest>,
) -> HttpResponse {
    if caller != Caller::Service {
        return json_error(StatusCode::FORBIDDEN, "Preflight is for the Zazu service");
    }

    use crate::schema::back_drivers::dsl::{back_drivers as drivers, *};

    const MIN_DRIVERS_REQUIRED: usize = 3;
    const MAX_RETRIES: usize = 4;

    let pick_up_point = req.pick_up.clone();
    let drop_off_point = req.drop_off.clone();

    let distance_km = distance_between(&pick_up_point, &drop_off_point);

    let estimated_time_min =
        crate::services::pricing::estimated_time_min(distance_km, &req.ride_type);

    let estimated_price = match req.ride_type {
        RideType::ASAP => crate::services::pricing::calculate_asap(distance_km, estimated_time_min),
        RideType::ASAPEXPRESS => crate::services::pricing::calculate_express(distance_km, estimated_time_min),
    };

    let vehicle_filter: Vec<String> = match req.ride_type {
        RideType::ASAP => vec!["EV".to_string()],
        RideType::ASAPEXPRESS => vec!["Bike".to_string()],
    };

    for _ in 0..MAX_RETRIES {
        let result = blocking(&pool, {
            let vehicle_filter = vehicle_filter.clone();
            move |conn| Ok(drivers
                .filter(vehicle_type.eq_any(&vehicle_filter))
                .filter(status.eq("available"))
                .limit(50)
                .select(Driver::as_select())
                .load::<Driver>(conn)?)
        }).await;

        let driver_list = match result {
            Ok(list) => list,
            Err(e) => return e.response(),
        };

        // A driver with no usable location can't count as nearby.
        let nearby_count = driver_list
            .iter()
            .filter_map(Driver::location)
            .filter(|location| minimum_distance_between_driver_and_pickup(pick_up_point.clone(), location.clone()))
            .count();

        if nearby_count >= MIN_DRIVERS_REQUIRED {
            return HttpResponse::Ok().json(RidePreflightResponse {
                can_serve: true,
                estimated_price,
                distance_km,
                estimated_time_min,
            });
        }
        sleep(Duration::from_millis(500)).await;
    }

    HttpResponse::Ok().json(RidePreflightResponse {
        can_serve: false,
        estimated_price,
        distance_km,
        estimated_time_min,
    })
}

pub async fn register_zazu_url(
    caller: Caller,
    body: web::Json<ZazuUrlRegistration>,
) -> HttpResponse {
    if caller != Caller::Service {
        return json_error(StatusCode::FORBIDDEN, "Only the Zazu service can register its URL");
    }
    let url = body.into_inner().url;
    *ZAZU_URL.lock().await = url.clone();
    println!("Zazu URL registered: {}", url);
    HttpResponse::Ok().json(message_body("Zazu URL registered"))
}

// Where the driver's share of each delivery is paid, so only the signed-in driver can set
// their own.
pub async fn update_bank_details(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<UpdateBankDetailsRequest>,
) -> HttpResponse {
    use crate::schema::back_drivers::dsl::{back_drivers as drivers, *};

    let me = auth.user_id;
    let req = body.into_inner();
    if req.driver_id.map_or(false, |id| id != me) {
        return json_error(StatusCode::FORBIDDEN, "You can only change your own bank details");
    }
    let (account, bank) = (req.account_number.trim().to_string(), req.bank_code.trim().to_string());
    if account.is_empty() || bank.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "account_number and bank_code are required");
    }

    let result = blocking(&pool, move |conn| {
        Ok(diesel::update(drivers.filter(driver_id.eq(me)))
            .set((
                account_number.eq(account),
                bank_code.eq(bank),
            ))
            .execute(conn)?)
    }).await;

    match result {
        Ok(rows) if rows > 0 => HttpResponse::Ok().json(message_body("Bank details updated")),
        Ok(_) => json_error(StatusCode::NOT_FOUND, "Driver not found"),
        Err(e) => e.response(),
    }
}

// ---------- Ride details for Zazu ----------

#[derive(QueryableByName)]
struct ZazuOrderDetails {
    #[diesel(sql_type = Nullable<SqlUuid>)]
    order_ref: Option<Uuid>,
    #[diesel(sql_type = Text)]
    order_code: String,
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    driver_id: Option<Uuid>,
    #[diesel(sql_type = Nullable<Double>)]
    pickup_lat: Option<f64>,
    #[diesel(sql_type = Nullable<Double>)]
    pickup_long: Option<f64>,
    #[diesel(sql_type = Nullable<Text>)]
    pickup_name: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pickup_contact_name: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pickup_contact_phone: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pickup_code: Option<String>,
    #[diesel(sql_type = Nullable<Double>)]
    dropoff_lat: Option<f64>,
    #[diesel(sql_type = Nullable<Double>)]
    dropoff_long: Option<f64>,
    #[diesel(sql_type = Nullable<Text>)]
    dropoff_name: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    dropoff_code: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    customer_phone: Option<String>,
}

// Tells Zazu who is carrying one of its orders, once a driver has taken it. Zazu writes this
// to telegram_orders and passes the codes on: the sender gives the pickup code to the driver,
// the receiver the drop-off code. Zazu runs no pickup or drop-off step itself; the driver app
// enters both codes here (confirm-pickup, confirm-dropoff). The driver's bank details go along
// because Zazu pays its own drivers.
// Runs after the driver's answer is committed. App orders are skipped. Sent again if the order
// is re-assigned, so Zazu should treat it as an upsert keyed by order_ref.
pub async fn forward_ride_details_to_zazu(pool: web::Data<DbPool>, order_id: i64, driver_uuid: Uuid) {
    let details = blocking(&pool, move |conn| {
        let order: Option<ZazuOrderDetails> = diesel::sql_query(
            "SELECT order_ref, order_code, status::text AS status, driver_id, \
                pickup_lat::float8 AS pickup_lat, pickup_long::float8 AS pickup_long, pickup_name, \
                pickup_contact_name, pickup_contact_phone, pickup_code, \
                dropoff_lat::float8 AS dropoff_lat, dropoff_long::float8 AS dropoff_long, dropoff_name, \
                dropoff_code, customer_phone \
             FROM app_delivery_orders WHERE id = $1 AND lower(source) = 'zazu'"
        )
        .bind::<BigInt, _>(order_id)
        .get_result(conn)
        .optional()?;
        let Some(order) = order else { return Ok(None) };
        let driver = crate::schema::back_drivers::table
            .find(driver_uuid)
            .select(Driver::as_select())
            .first::<Driver>(conn)?;
        Ok(Some((order, driver)))
    }).await;

    let (order, driver) = match details {
        Ok(Some(details)) => details,
        Ok(None) => return,
        Err(e) => {
            eprintln!("Order {}: ride details for Zazu not sent, could not load them: {}", order_id, e.message());
            return;
        }
    };
    // The order moved on (re-assigned, cancelled) before this ran; the newer state is what counts.
    if order.driver_id != Some(driver_uuid) {
        return;
    }

    let location = driver.location();
    let payload = serde_json::json!({
        "order_ref":  order.order_ref,
        "order_code": order.order_code,
        "status":     order.status,
        "pick_up": {
            "lat":           order.pickup_lat,
            "lng":           order.pickup_long,
            "name":          order.pickup_name,
            "contact_name":  order.pickup_contact_name,
            "contact_phone": order.pickup_contact_phone,
            "code":          order.pickup_code,
        },
        "drop_off": {
            "lat":            order.dropoff_lat,
            "lng":            order.dropoff_long,
            "name":           order.dropoff_name,
            "customer_phone": order.customer_phone,
            "code":           order.dropoff_code,
        },
        "driver": {
            "driver_id":      driver.driver_id,
            "name":           driver.name,
            "phone":          driver.phone,
            "vehicle_type":   driver.vehicle_type,
            "vehicle":        driver.vehicle,
            "location":       location.map(|at| serde_json::json!({ "lat": at.lat, "lng": at.lng })),
            "account_number": driver.account_number,
            "bank_code":      driver.bank_code,
        },
    });

    if let Err(e) = zazu::post("/ride-details", &payload).await {
        eprintln!("Order {}: ride details for Zazu not delivered: {}", order_id, e);
    }
}

// ---------- Codes: pickup, drop-off, return ----------
//
// Every code is entered in the driver app and checked here, for app and Zazu orders alike.
// Each is checked only in its own step, against its own column, and every step is a guarded
// update on the status it expects, so a code can't be used out of order or twice. The codes
// of one order never equal each other (codes::code_pair, confirmation_code_except).

// The order the signed-in driver is carrying.
fn drivers_order(conn: &mut PgConnection, order_ref: &str, me: Uuid, lock: bool) -> Result<OrderRow, ApiError> {
    load_order(conn, OrderKey::parse(order_ref), lock)?
        .filter(|order| order.driver_id == Some(me))
        .ok_or_else(|| ApiError::NotFound("No order of yours for this order_ref".into()))
}

fn check_code(expected: Option<&str>, sent: Option<&str>, what: &str) -> Result<(), ApiError> {
    let sent = sent.map(str::trim).filter(|code| !code.is_empty())
        .ok_or_else(|| ApiError::BadRequest(format!("{} code is required", what)))?;
    let expected = expected.ok_or_else(|| ApiError::Conflict(format!("Order has no {} code", what)))?;
    if sent != expected {
        return Err(ApiError::Forbidden(format!("Wrong {} code", what)));
    }
    Ok(())
}

// The driver has to be at `place` (within riders::SAME_PLACE_KM) by their last reported location.
fn require_near(conn: &mut PgConnection, driver_uuid: Uuid, place: Option<GeoPoint>, what: &str) -> Result<(), ApiError> {
    use crate::schema::back_drivers::dsl as d;

    let location: serde_json::Value = d::back_drivers
        .find(driver_uuid)
        .select(d::driver_location)
        .first(conn)?;
    let here: GeoPoint = serde_json::from_value(location)
        .map_err(|_| ApiError::Conflict("Your location is unknown; turn on location and try again".into()))?;
    if !same_place(place, &here) {
        return Err(ApiError::Conflict(format!("You are not at the {} yet", what)));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct ConfirmPickupRequest {
    // order_ref UUID or the order_code.
    pub order_ref: String,
    #[serde(default)]
    pub pickup_code: Option<String>,
    // Ignored: the driver is whoever the token belongs to. A different value is rejected.
    #[serde(default)]
    pub driver_id: Option<Uuid>,
}

#[derive(Serialize)]
pub struct StepConfirmed {
    pub order_id: i64,
    pub status: &'static str,
}

// The driver enters the pickup code at the pickup: arriving_pickup/driver_arrived -> in_transit.
// Same for app and Zazu orders. Repeating a pickup that already went through is a no-op 200.
pub async fn confirm_pickup(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<ConfirmPickupRequest>,
) -> HttpResponse {
    let me = auth.user_id;
    let req = body.into_inner();
    if req.driver_id.map_or(false, |id| id != me) {
        return json_error(StatusCode::FORBIDDEN, "driver_id does not match the signed-in driver");
    }

    let result = blocking(&pool, move |conn| conn.transaction(|conn| {
        let order = drivers_order(conn, &req.order_ref, me, true)?;
        match order.status.as_str() {
            "in_transit" if order.is_pickup_code_authenticated => return Ok(order.id),
            "arriving_pickup" | "driver_arrived" => {},
            other => return Err(ApiError::Conflict(format!("Order is {}, not waiting for pickup", other))),
        }
        check_code(order.pickup_code.as_deref(), req.pickup_code.as_deref(), "pickup")?;

        diesel::sql_query(
            "UPDATE app_delivery_orders SET status = 'in_transit', is_pickup_code_authenticated = true, \
                pickup_time = now(), modified_at = now() \
             WHERE id = $1 AND driver_id = $2 AND status IN ('arriving_pickup', 'driver_arrived')"
        )
        .bind::<BigInt, _>(order.id)
        .bind::<SqlUuid, _>(me)
        .execute(conn)?;
        open_trip(conn, order.id)?;
        Ok(order.id)
    })).await;

    match result {
        Ok(order_id) => {
            zazu::spawn_notify_status(pool.clone(), order_id);
            HttpResponse::Ok().json(StepConfirmed { order_id, status: "in_transit" })
        }
        Err(e) => e.response(),
    }
}

#[derive(Deserialize)]
pub struct ConfirmDropoffRequest {
    // order_ref UUID or the order_code.
    pub order_ref: String,
    #[serde(default)]
    pub dropoff_code: Option<String>,
}

#[derive(Serialize)]
pub struct DropoffConfirmed {
    pub order_id: i64,
    pub status: &'static str,
    // The driver's share of an app order; not_applicable for Zazu, which pays its own drivers.
    pub driver_credit: DriverCredit,
}

// The driver enters the drop-off code at the drop-off, for app and Zazu orders alike. Ends the
// delivery: in_transit -> delivered and the driver's slot freed. A Zazu trip is Completed here;
// an app order's driver is then paid their share, and its trip is Completed once Korapay
// confirms that payout (here, in the follow-up checks, or in the sweep). Calling it again on a
// delivered order only re-checks the payout, which Korapay never pays twice (fixed dr_ reference).
pub async fn confirm_dropoff(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<ConfirmDropoffRequest>,
) -> HttpResponse {
    let me = auth.user_id;
    let req = body.into_inner();

    // (order id, order_ref when this call completed a Zazu trip)
    let result = blocking(&pool, move |conn| conn.transaction(|conn| {
        let order = drivers_order(conn, &req.order_ref, me, true)?;
        match order.status.as_str() {
            "delivered" => return Ok((order.id, None)),
            "in_transit" => {},
            other => return Err(ApiError::Conflict(format!("Order is {}, not in transit", other))),
        }
        check_code(order.dropoff_code.as_deref(), req.dropoff_code.as_deref(), "drop-off")?;
        require_near(conn, me, order.drop_off(), "drop-off")?;

        diesel::sql_query(
            "UPDATE app_delivery_orders SET status = 'delivered', is_dropoff_code_authenticated = true, \
                dropoff_time = now(), modified_at = now() \
             WHERE id = $1 AND driver_id = $2 AND status = 'in_transit'"
        )
        .bind::<BigInt, _>(order.id)
        .bind::<SqlUuid, _>(me)
        .execute(conn)?;

        let completed = match order.source() {
            OrderSource::Zazu => close_trip(conn, order.order_ref, "Completed")?.then_some(order.order_ref).flatten(),
            OrderSource::App => {
                await_payout(conn, order.order_ref)?;
                None
            }
        };
        refresh_driver_status(conn, me)?;
        Ok((order.id, completed))
    })).await;

    let order_id = match result {
        Ok((order_id, completed)) => {
            if let Some(order_ref) = completed {
                escrow::spawn_record_ride(pool.clone(), order_ref, None);
            }
            zazu::spawn_notify_status(pool.clone(), order_id);
            order_id
        }
        Err(e) => return e.response(),
    };

    // After the commit: the order is delivered whatever the payout does.
    let driver_credit = korapay::credit_driver(&pool, order_id).await;
    match driver_credit.status {
        CreditStatus::Processing => korapay::follow_payout(pool.clone(), order_id),
        CreditStatus::Failed =>
            eprintln!("Order {}: delivered, driver not credited: {}", order_id, driver_credit.error.as_deref().unwrap_or("")),
        CreditStatus::Paid | CreditStatus::NotApplicable => {},
    }
    HttpResponse::Ok().json(DropoffConfirmed { order_id, status: "delivered", driver_credit })
}

#[derive(QueryableByName)]
struct ReturnRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    driver_id: Option<Uuid>,
    #[diesel(sql_type = Nullable<Text>)]
    return_code: Option<String>,
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = Nullable<Double>)]
    return_lat: Option<f64>,
    #[diesel(sql_type = Nullable<Double>)]
    return_long: Option<f64>,
}

#[derive(Deserialize)]
pub struct ConfirmReturnRequest {
    // order_ref UUID or the order_code.
    pub order_ref: String,
    #[serde(default)]
    pub return_code: Option<String>,
}

// App orders only (Zazu orders have no returns). The package of a cancelled order is back at
// the pickup: the driver enters the return code there, the return closes and its slot is freed.
pub async fn confirm_return(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<ConfirmReturnRequest>,
) -> HttpResponse {
    let me = auth.user_id;
    let req = body.into_inner();

    let result = blocking(&pool, move |conn| conn.transaction(|conn| {
        let order = drivers_order(conn, &req.order_ref, me, false)?;
        if order.source() != OrderSource::App {
            return Err(ApiError::NotFound("Zazu orders have no returns".into()));
        }
        let ret: ReturnRow = diesel::sql_query(
            "SELECT id, driver_id, return_code, status, \
                return_lat::float8 AS return_lat, return_long::float8 AS return_long \
             FROM app_returned WHERE order_id = $1 FOR UPDATE"
        )
        .bind::<BigInt, _>(order.id)
        .get_result::<ReturnRow>(conn)
        .optional()?
        .filter(|ret| ret.driver_id == Some(me))
        .ok_or_else(|| ApiError::NotFound("No return of yours for this order".into()))?;

        if ret.status == "returned" {
            return Ok(order.id);
        }
        check_code(ret.return_code.as_deref(), req.return_code.as_deref(), "return")?;
        let pickup = match (ret.return_lat, ret.return_long) {
            (Some(lat), Some(lng)) => Some(GeoPoint { lat, lng, name: None }),
            _ => None,
        };
        require_near(conn, me, pickup, "pickup")?;

        diesel::sql_query(
            "UPDATE app_returned SET status = 'returned', is_return_code_authenticated = true, returned_at = now() \
             WHERE id = $1 AND status = 'returning'"
        )
        .bind::<BigInt, _>(ret.id)
        .execute(conn)?;

        refresh_driver_status(conn, me)?;
        Ok(order.id)
    })).await;

    match result {
        Ok(order_id) => HttpResponse::Ok().json(StepConfirmed { order_id, status: "returned" }),
        Err(e) => e.response(),
    }
}

pub fn routes() -> Scope {
    web::scope("/drivers")
        .route("/driver-response", web::post().to(driver_response_handler))
        .route("/update-driver", web::post().to(update_driver))
        .route("/update-status", web::post().to(update_driver_status))
        .route("/ride-preflight", web::post().to(preflight_check))
        .route("/update-bank-details", web::put().to(update_bank_details))
        .route("/confirm-pickup", web::post().to(confirm_pickup))
        .route("/confirm-dropoff", web::post().to(confirm_dropoff))
        .route("/confirm-return", web::post().to(confirm_return))
        .route("/register-zazu-url", web::post().to(register_zazu_url))
}

#[derive(Deserialize)]
pub struct DriverResponsePayload {
    pub offer_id: Uuid,
    pub accepted: bool,
}

#[derive(Serialize)]
pub struct OfferAnswer {
    pub offer_id: Uuid,
    pub order_id: i64,
    // accepted, rejected, or started (assigned delivery)
    pub status: &'static str,
}

#[derive(Deserialize, Serialize, Clone)]
pub enum DriverResponse {
    Accepted,
    Rejected,
    Timeout,
}

#[derive(Deserialize)]
pub struct RidePreflightRequest {
    pub rider_id: Uuid,
    pub pick_up: GeoPoint,
    pub drop_off: GeoPoint,
    pub ride_type: RideType,
}

#[derive(Serialize, Clone)]
pub struct RidePreflightResponse {
    pub can_serve: bool,
    pub estimated_price: i64,
    pub distance_km: f64,
    pub estimated_time_min: i32
}

#[derive(Debug, Queryable, Serialize, Deserialize, Selectable, Clone)]
#[diesel(table_name = crate::schema::back_drivers)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Driver {
    pub driver_id: Uuid,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub driver_pubkey: serde_json::Value,
    pub name: String,
    pub email: String,
    pub phone: String,
    pub status: String,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub driver_location: serde_json::Value,
    pub license_number: Option<String>,
    pub vehicle_type: String,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub driver_response: serde_json::Value,
    pub vehicle: Option<String>,
    pub account_number: Option<String>,
    pub bank_code: Option<String>,
}

impl Driver {

    fn allowed_vehicle_types(&self, ride_type: &RideType) -> Vec<String> {
        match ride_type {
            RideType::ASAP => vec!["EV".to_string()],
            RideType::ASAPEXPRESS => vec!["Bike".to_string()],
        }
    }

    fn can_take_ride(&self, ride_type: &RideType) -> bool {
        let allowed = self.allowed_vehicle_types(ride_type);
        allowed.contains(&self.vehicle_type)
    }

    // None until the driver app has reported a usable location.
    pub fn location(&self) -> Option<GeoPoint> {
        serde_json::from_value(self.driver_location.clone()).ok()
    }
}

#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::back_drivers)]
pub struct DriverUpdate {
    pub driver_location: serde_json::Value,
    pub driver_response: serde_json::Value,
}

#[derive(Deserialize)]
pub struct ZazuUrlRegistration {
    pub url: String,
}

#[derive(Deserialize)]
pub struct UpdateBankDetailsRequest {
    // Ignored: always the signed-in driver. A different value is rejected.
    #[serde(default)]
    pub driver_id: Option<Uuid>,
    pub account_number: String,
    pub bank_code: String,
}

// app_ride_offers is owned by ASAP_APPS. Inserting a row fires notify_ride_offer(),
// which pushes the offer to the driver's device.
#[derive(Debug, Clone, Serialize, Deserialize, Queryable, Selectable)]
#[diesel(table_name = crate::schema::app_ride_offers)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct RideOffer {
    pub id: Uuid,
    pub order_id: i64,
    pub driver_id: Uuid,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub responded_at: Option<DateTime<Utc>>,
}

// id, status and created_at use the table defaults (gen_random_uuid(), 'pending', now()).
// expires_at is set alongside it from the DB clock (see riders::expires_in).
#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = crate::schema::app_ride_offers)]
pub struct NewRideOffer {
    pub order_id: i64,
    pub driver_id: Uuid,
}
