use actix_web::{ web, Scope, HttpResponse, http::StatusCode };
use serde::{ Deserialize, Serialize };
use tokio::sync::{Mutex, oneshot};
use lazy_static::lazy_static;
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::dsl::sql;
use diesel::expression::SqlLiteral;
use diesel::sql_types::{ Nullable, Timestamptz };
use uuid::Uuid;
use chrono::{ DateTime, Utc };
use tokio::time::{ Duration, Instant, sleep, sleep_until, timeout };
use std::collections::{ HashMap, HashSet };
use std::env;
use crate::db::{ DbPool };
use crate::api::admin::Rider;
use crate::api::errors::{ ApiError, blocking, json_error };
use crate::api::orders::{ NewOrder, OrderKey, OrderRow, ensure_codes, insert_order, load_order, paid_amount };
use crate::services::pricing::{ GeoPoint, distance_between };
use crate::services::pricing;
use crate::services::phone::normalize_phone;
use crate::services::korapay::{ self, RefundOutcome };
use crate::services::zazu;
use crate::services::areas::require_service_area;
use crate::services::notifications::calculate_eta;
use crate::api::drivers::{ Driver, NewRideOffer, AssignOutcome, assign_order, max_active_orders, refresh_driver_status, returning_counts };
use crate::services::auth::{ AnyAuthGuard, Caller };
use crate::schema::sql_types::DeliveryStatus;


lazy_static! {
    // Wakes the search waiting on a driver, keyed by driver id. Only a fast path:
    // app_ride_offers is the source of truth, so a lost signal just means the search
    // reads the row when the offer's window closes.
    pub static ref DRIVER_RESPONSES: Mutex<HashMap<Uuid, oneshot::Sender<()>>> = Mutex::new(HashMap::new());
    // Orders with a search running, so a retried request doesn't start a second one.
    static ref ACTIVE_SEARCHES: std::sync::Mutex<HashSet<i64>> = std::sync::Mutex::new(HashSet::new());
}

// How long a driver has to accept or decline an offer.
const OFFER_TTL_SECS: u64 = 50;
// How long an assigned driver has to tap Start before the order is taken back.
const ASSIGNED_START_SECS: u64 = 60;
// How long a search runs before the order is marked no_driver. The customer app's
// SEARCH_TIMEOUT_MS should wait at least this long.
const SEARCH_WINDOW_SECS: u64 = 1800;
// Pause between rounds, so an empty area doesn't burn through the search in seconds.
const ROUND_PAUSE_SECS: u64 = 7;
const DRIVERS_PER_ROUND: usize = 10;
// How far from the pickup drivers are looked for. A search starts at the step for its attempt
// (the first search, then each Try again after no_driver) and moves out one step every
// RADIUS_STEP_SECS while it runs, staying at the widest. App and Zazu orders alike.
const SEARCH_RADIUS_KM: [f64; 3] = [5.0, 8.0, 12.0];
const RADIUS_STEP_SECS: u64 = 180;
// A driver who declined or let an offer lapse is offered the same order again after this,
// if they're still free and in range.
const RETRY_DRIVER_AFTER_MINS: u32 = 10;

fn search_radius_km(attempt: i32, elapsed_secs: u64) -> f64 {
    let start = attempt.max(1) as usize - 1;
    let steps = (elapsed_secs / RADIUS_STEP_SECS) as usize;
    SEARCH_RADIUS_KM[(start + steps).min(SEARCH_RADIUS_KM.len() - 1)]
}

// delivery_status is a Postgres enum; only fixed literals are passed here, never user input.
pub fn delivery_status(value: &'static str) -> SqlLiteral<DeliveryStatus> {
    sql::<DeliveryStatus>(&format!("'{}'", value))
}

// Uses the DB clock so expiry checks never depend on this server's clock.
fn expires_in(secs: u64) -> SqlLiteral<Timestamptz> {
    sql::<Timestamptz>(&format!("now() + interval '{} seconds'", secs))
}

pub fn db_now_nullable() -> SqlLiteral<Nullable<Timestamptz>> {
    sql::<Nullable<Timestamptz>>("now()")
}

pub async fn with_conn<T, F>(pool: &web::Data<DbPool>, f: F) -> Result<T, String>
where
    F: FnOnce(&mut PgConnection) -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let pool = pool.clone();
    web::block(move || {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        f(&mut conn)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum DispatchMode {
    // 50-second accept/decline window per driver.
    Offer,
    // Handed straight to one driver, who can only tap Start.
    Assigned,
}

// Which dispatch an order gets is policy, not code: sources listed in ASSIGNED_DISPATCH_SOURCES
// (comma separated, default "zazu") are assigned, everything else is offered.
fn dispatch_mode(source: &str) -> DispatchMode {
    let assigned = env::var("ASSIGNED_DISPATCH_SOURCES").unwrap_or_else(|_| "zazu".to_string());
    if assigned.split(',').any(|s| s.trim().eq_ignore_ascii_case(source)) {
        DispatchMode::Assigned
    } else {
        DispatchMode::Offer
    }
}

pub fn find_order_id(connection: &mut PgConnection, order_ref: Uuid) -> QueryResult<Option<i64>> {
    use crate::schema::app_delivery_orders::dsl as o;

    o::app_delivery_orders
        .filter(o::order_ref.eq(order_ref))
        .select(o::id)
        .first::<i64>(connection)
        .optional()
}

fn find_ride_request(connection: &mut PgConnection, order_ref: Uuid) -> QueryResult<Option<RideRequest>> {
    use crate::schema::back_ride_request::dsl as r;

    r::back_ride_request
        .filter(r::order_ref.eq(order_ref))
        .select(RideRequest::as_select())
        .first::<RideRequest>(connection)
        .optional()
}

// Puts the order into pending for a fresh search. Refuses if a driver already has it
// or it has moved on (awaiting payment, in transit, cancelled, ...).
fn reopen_order(connection: &mut PgConnection, order_id: i64) -> Result<(), ApiError> {
    use crate::schema::app_delivery_orders::dsl as o;

    let reopened = diesel::update(
        o::app_delivery_orders
            .filter(o::id.eq(order_id))
            .filter(o::driver_id.is_null())
            .filter(o::status.eq(delivery_status("pending")).or(o::status.eq(delivery_status("no_driver"))))
    )
    .set((
        o::status.eq(delivery_status("pending")),
        o::modified_at.eq(diesel::dsl::now),
    ))
    .execute(connection)?;

    if reopened == 0 {
        return Err(ApiError::Conflict("Order already has a driver or is closed".into()));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct SearchStarted {
    pub status: &'static str,
    pub order_id: i64,
    pub estimated_price: i64,
    pub estimated_arrival: String,
}

// Removes the order from ACTIVE_SEARCHES however the search ends, panics included.
struct SearchGuard(i64);

impl Drop for SearchGuard {
    fn drop(&mut self) {
        ACTIVE_SEARCHES.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

// Starts the background search unless one is already running for the order.
pub fn start_search(pool: web::Data<DbPool>, order_id: i64) {
    spawn_search(pool, order_id, None);
}

// `resume`: how long this order has already been searching (see sweep_orphaned_searches);
// None for a new search, which also counts as an attempt.
fn spawn_search(pool: web::Data<DbPool>, order_id: i64, resume: Option<u64>) -> bool {
    if !ACTIVE_SEARCHES.lock().unwrap_or_else(|e| e.into_inner()).insert(order_id) {
        return false;
    }
    let guard = SearchGuard(order_id);
    tokio::spawn(async move {
        let _guard = guard;
        run_assign_driver(pool, order_id, resume).await;
    });
    true
}

// ---------- Orphaned-search sweep ----------
//
// A search lives in this server's memory, so a restart (or a crash) leaves its order pending
// with nothing looking for a driver. The sweep finds pending orders with no driver and no
// search running here and resumes them: not a new attempt, at the radius already reached, for
// what's left of SEARCH_WINDOW_SECS (an order already past it is marked no_driver at once).
// The time searched so far is measured from modified_at, set when the order went pending.
// Assumes one server instance: a second instance would see the first's searches as orphaned.

const SEARCH_SWEEP_EVERY_SECS: u64 = 60;
// Leaves alone an order that just went pending, whose request is still starting its search.
const ORPHAN_AFTER_SECS: i32 = 60;

#[derive(QueryableByName)]
struct PendingOrder {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    searched_secs: i64,
}

fn pending_without_driver(connection: &mut PgConnection) -> QueryResult<Vec<PendingOrder>> {
    diesel::sql_query(
        "SELECT o.id, extract(epoch FROM now() - o.modified_at)::int8 AS searched_secs \
           FROM app_delivery_orders o \
          WHERE o.status = 'pending' AND o.driver_id IS NULL \
            AND o.modified_at < now() - make_interval(secs => $1) \
            AND EXISTS (SELECT 1 FROM back_ride_request r WHERE r.order_ref = o.order_ref) \
          ORDER BY o.modified_at \
          LIMIT 200"
    )
    .bind::<diesel::sql_types::Integer, _>(ORPHAN_AFTER_SECS)
    .load(connection)
}

async fn sweep_orphaned_searches(pool: &web::Data<DbPool>) {
    let pending = match with_conn(pool, |conn| pending_without_driver(conn).map_err(|e| e.to_string())).await {
        Ok(pending) => pending,
        Err(e) => return eprintln!("Search sweep: could not load pending orders: {}", e),
    };
    for order in pending {
        if spawn_search(pool.clone(), order.id, Some(order.searched_secs.max(0) as u64)) {
            println!("Search sweep: order {} had no search running ({}s in); resumed", order.id, order.searched_secs);
        }
    }
}

pub fn spawn_search_sweep(pool: web::Data<DbPool>) {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(SEARCH_SWEEP_EVERY_SECS));
        loop {
            ticks.tick().await;
            sweep_orphaned_searches(&pool).await;
        }
    });
}

fn search_started(order_id: i64, estimated_price: i64, estimated_time_min: i32) -> HttpResponse {
    HttpResponse::Ok().json(SearchStarted {
        status: "success",
        order_id,
        estimated_price,
        estimated_arrival: calculate_eta(estimated_time_min),
    })
}


#[derive(Deserialize)]
pub struct AssignDriverRequest {
    pub order_ref: Uuid,
}

// "Try again" after no_driver: restarts the driver search on the same order. The customer may
// only retry their own app order, and only while its payment stands; Zazu retries come from
// the Zazu service.
pub async fn assign_driver_handler(
    caller: Caller,
    body: web::Json<AssignDriverRequest>,
    pool: web::Data<DbPool>,
) -> HttpResponse {
    let order_ref = body.into_inner().order_ref;

    let loaded = blocking(&pool, move |conn| {
        let order = load_order(conn, OrderKey::Ref(order_ref), false)?
            .ok_or_else(|| ApiError::NotFound("No delivery order for this order_ref".into()))?;
        let request = find_ride_request(conn, order_ref)?
            .ok_or_else(|| ApiError::NotFound("No ride request for this order_ref".into()))?;
        Ok((order, request))
    }).await;
    let (order, request) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => return e.response(),
    };

    let allowed = match (caller, order.source()) {
        (Caller::User(me), OrderSource::App) if order.client_id == me =>
            paid_app_order(&order, request.estimated_price).await.map(|_| ()),
        (Caller::User(_), OrderSource::App) => Err(ApiError::NotFound("No delivery order for this order_ref".into())),
        (Caller::User(_), OrderSource::Zazu) => Err(ApiError::Forbidden("Zazu orders are retried by the Zazu service".into())),
        (Caller::Service, OrderSource::Zazu) => Ok(()),
        (Caller::Service, OrderSource::App) => Err(ApiError::Forbidden("App orders are retried by the customer".into())),
    };
    if let Err(e) = allowed {
        return e.response();
    }

    let order_id = order.id;
    match blocking(&pool, move |conn| reopen_order(conn, order_id)).await {
        Ok(()) => {
            start_search(pool, order_id);
            search_started(order_id, request.estimated_price, request.estimated_time_min)
        }
        Err(e) => e.response(),
    }
}

// An app order's payment, checked with Korapay: the reference must be on the row, the charge
// must have succeeded, and it must cover `price`. Returns what was paid, in naira.
async fn paid_app_order(order: &OrderRow, price: i64) -> Result<i64, ApiError> {
    let reference = order.payment_reference.as_deref().ok_or_else(|| ApiError::PaymentRequired(
        "Order has no payment; pay through /payments/korapay/initialize first".into()
    ))?;
    let paid = paid_amount(reference).await
        .map_err(|e| ApiError::BadGateway(format!("Could not check the payment: {}", e)))?
        .ok_or_else(|| ApiError::PaymentRequired("Payment has not been completed".into()))?;
    if price > paid {
        return Err(ApiError::PaymentRequired(format!("Paid {} but this request costs {}", paid, price)));
    }
    Ok(paid)
}

struct SearchInput {
    pick_up: GeoPoint,
    vehicle_types: Vec<String>,
    mode: DispatchMode,
    // Which search of this order this is; sets the starting radius (search_radius_km).
    attempt: i32,
}

// A new search also counts as an attempt against the ride request, which sets its radius; a
// resumed one carries on as the attempt it was.
fn load_search_input(connection: &mut PgConnection, order_id: i64, new_attempt: bool) -> Result<SearchInput, String> {
    use crate::schema::app_delivery_orders::dsl as o;
    use crate::schema::back_ride_request::dsl as r;

    let (order_ref, source) = o::app_delivery_orders
        .find(order_id)
        .select((o::order_ref, o::source))
        .first::<(Option<Uuid>, String)>(connection)
        .map_err(|e| e.to_string())?;
    let order_ref = order_ref.ok_or("order has no order_ref")?;

    let request = r::back_ride_request.filter(r::order_ref.eq(order_ref));
    let found = if new_attempt {
        diesel::update(request)
            .set(r::search_attempts.eq(r::search_attempts + 1))
            .returning((r::pick_up, r::ride_type, r::search_attempts))
            .get_result::<(serde_json::Value, serde_json::Value, i32)>(connection)
    } else {
        request
            .select((r::pick_up, r::ride_type, r::search_attempts))
            .first::<(serde_json::Value, serde_json::Value, i32)>(connection)
    };
    let (pick_up, ride_type, attempt) = found.map_err(|e| format!("no ride request for order: {}", e))?;

    let pick_up: GeoPoint = serde_json::from_value(pick_up).map_err(|e| format!("bad pick_up: {}", e))?;
    let ride_type: RideType = serde_json::from_value(ride_type).map_err(|e| format!("bad ride_type: {}", e))?;

    Ok(SearchInput {
        pick_up,
        vehicle_types: ride_type.vehicle_types(),
        mode: dispatch_mode(&source),
        attempt,
    })
}

fn order_still_searching(connection: &mut PgConnection, order_id: i64) -> Result<bool, String> {
    use crate::schema::app_delivery_orders::dsl as o;

    diesel::select(diesel::dsl::exists(
        o::app_delivery_orders
            .filter(o::id.eq(order_id))
            .filter(o::driver_id.is_null())
            .filter(o::status.eq(delivery_status("pending")))
    ))
    .get_result::<bool>(connection)
    .map_err(|e| e.to_string())
}

// The closest drivers who can take the order, nearest first, capped at DRIVERS_PER_ROUND.
// Eligible means online with a free slot by their live order count (busy is included because
// the stored status can lag a delivery finishing in the app), in range of the pickup, not
// passed over for this order in the last RETRY_DRIVER_AFTER_MINS, and not holding a live offer
// for another order.
// Drivers whose last position is older than this are left out of the search.
const LOCATION_STALE_MINS: u32 = 10;

fn load_candidates(connection: &mut PgConnection, order_id: i64, input: &SearchInput, radius_km: f64) -> Result<Vec<Driver>, String> {
    use crate::schema::back_drivers::dsl as d;
    use crate::schema::app_ride_offers::dsl as ro;
    use crate::schema::app_delivery_orders::dsl as o;

    let excluded: HashSet<Uuid> = ro::app_ride_offers
        .filter(
            ro::order_id.eq(order_id)
                .and(ro::status.eq_any(vec!["rejected", "expired"]))
                .and(ro::created_at.gt(sql::<Timestamptz>(&format!(
                    "now() - interval '{} minutes'", RETRY_DRIVER_AFTER_MINS
                ))))
            .or(ro::status.eq("pending").and(ro::expires_at.gt(diesel::dsl::now)))
        )
        .select(ro::driver_id)
        .load::<Uuid>(connection)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();

    let mut nearby: Vec<(f64, Driver)> = d::back_drivers
        .filter(d::vehicle_type.eq_any(&input.vehicle_types))
        .filter(d::status.eq_any(vec!["available", "busy"]))
        // A driver whose app stopped reporting may be far from where the row says.
        .filter(sql::<diesel::sql_types::Bool>(&format!(
            "location_updated_at > now() - interval '{} minutes'", LOCATION_STALE_MINS
        )))
        .select(Driver::as_select())
        .load::<Driver>(connection)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|driver| !excluded.contains(&driver.driver_id))
        .filter_map(|driver| {
            let location: GeoPoint = serde_json::from_value(driver.driver_location.clone()).ok()?;
            let distance = distance_between(&location, &input.pick_up);
            (distance <= radius_km).then_some((distance, driver))
        })
        .collect();

    if nearby.is_empty() {
        return Ok(Vec::new());
    }

    let ids: Vec<Uuid> = nearby.iter().map(|(_, driver)| driver.driver_id).collect();
    let mut loads: HashMap<Uuid, i64> = o::app_delivery_orders
        .filter(o::driver_id.eq_any(ids.clone()))
        .filter(o::status.eq(delivery_status("arriving_pickup")).or(o::status.eq(delivery_status("in_transit"))))
        .group_by(o::driver_id)
        .select((o::driver_id, diesel::dsl::count_star()))
        .load::<(Option<Uuid>, i64)>(connection)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|(id, count)| id.map(|id| (id, count)))
        .collect();
    // A package being taken back to its pickup still occupies the slot.
    for (id, count) in returning_counts(connection, &ids).map_err(|e| e.to_string())? {
        *loads.entry(id).or_insert(0) += count;
    }

    nearby.retain(|(_, driver)| {
        loads.get(&driver.driver_id).copied().unwrap_or(0) < max_active_orders(&driver.vehicle_type)
    });
    nearby.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(nearby.into_iter().take(DRIVERS_PER_ROUND).map(|(_, driver)| driver).collect())
}

// Offer mode: offers the order to one driver and waits for accept or decline. Inserting the
// row fires the push to the driver's device. Returns whether the driver accepted, read from
// the row itself; an accept has already assigned the order (see driver_response_handler).
async fn offer_to_driver(pool: &web::Data<DbPool>, order_id: i64, driver_uuid: Uuid) -> Result<bool, String> {
    // Registered before the insert so an instant answer can't beat it.
    let (tx, rx) = oneshot::channel();
    DRIVER_RESPONSES.lock().await.insert(driver_uuid, tx);

    let inserted = with_conn(pool, move |conn| {
        use crate::schema::app_ride_offers::dsl as ro;
        diesel::insert_into(ro::app_ride_offers)
            .values((
                NewRideOffer { order_id, driver_id: driver_uuid },
                ro::expires_at.eq(expires_in(OFFER_TTL_SECS)),
            ))
            .returning(ro::id)
            .get_result::<Uuid>(conn)
            .map_err(|e| e.to_string())
    }).await;

    let offer_id = match inserted {
        Ok(id) => id,
        Err(e) => {
            DRIVER_RESPONSES.lock().await.remove(&driver_uuid);
            return Err(format!("could not create offer: {}", e));
        }
    };

    wait_for_driver(rx, OFFER_TTL_SECS).await;
    DRIVER_RESPONSES.lock().await.remove(&driver_uuid);

    with_conn(pool, move |conn| {
        use crate::schema::app_ride_offers::dsl as ro;

        // Only lapses an unanswered offer; an answer that landed first wins.
        diesel::update(
            ro::app_ride_offers
                .filter(ro::id.eq(offer_id))
                .filter(ro::status.eq("pending"))
        )
        .set(ro::status.eq("expired"))
        .execute(conn)
        .map_err(|e| e.to_string())?;

        let status = ro::app_ride_offers
            .find(offer_id)
            .select(ro::status)
            .first::<String>(conn)
            .map_err(|e| e.to_string())?;

        Ok(status == "accepted")
    }).await
}

enum AssignAttempt {
    Started,
    NotStarted,
    Skipped(AssignOutcome),
}

// Assigned mode: hands the order straight to one driver and records it as an accepted offer
// (responded_at stays empty until they tap Start). If they don't start within
// ASSIGNED_START_SECS, the order is taken back so the search can move on.
async fn assign_to_driver(pool: &web::Data<DbPool>, order_id: i64, driver_uuid: Uuid) -> Result<AssignAttempt, String> {
    let (tx, rx) = oneshot::channel();
    DRIVER_RESPONSES.lock().await.insert(driver_uuid, tx);

    let assigned = with_conn(pool, move |conn| {
        use crate::schema::app_ride_offers::dsl as ro;

        conn.transaction(|conn| {
            let outcome = assign_order(conn, order_id, driver_uuid)?;
            if outcome != AssignOutcome::Assigned {
                return Ok(Err(outcome));
            }
            // Inserted after the assignment so the push only goes out for an order that is theirs.
            let offer_id = diesel::insert_into(ro::app_ride_offers)
                .values((
                    NewRideOffer { order_id, driver_id: driver_uuid },
                    ro::status.eq("accepted"),
                    ro::expires_at.eq(expires_in(ASSIGNED_START_SECS)),
                ))
                .returning(ro::id)
                .get_result::<Uuid>(conn)?;
            Ok(Ok(offer_id))
        })
        .map_err(|e: diesel::result::Error| e.to_string())
    }).await;

    let offer_id = match assigned {
        Ok(Ok(id)) => id,
        Ok(Err(outcome)) => {
            DRIVER_RESPONSES.lock().await.remove(&driver_uuid);
            return Ok(AssignAttempt::Skipped(outcome));
        }
        Err(e) => {
            DRIVER_RESPONSES.lock().await.remove(&driver_uuid);
            return Err(format!("could not assign: {}", e));
        }
    };

    wait_for_driver(rx, ASSIGNED_START_SECS).await;
    DRIVER_RESPONSES.lock().await.remove(&driver_uuid);

    with_conn(pool, move |conn| {
        use crate::schema::app_ride_offers::dsl as ro;
        use crate::schema::app_delivery_orders::dsl as o;

        conn.transaction(|conn| {
            // Only takes back a delivery that was never started; a Start that landed first wins.
            let lapsed = diesel::update(
                ro::app_ride_offers
                    .filter(ro::id.eq(offer_id))
                    .filter(ro::status.eq("accepted"))
                    .filter(ro::responded_at.is_null())
            )
            .set(ro::status.eq("expired"))
            .execute(conn)?;

            if lapsed == 0 {
                return Ok(AssignAttempt::Started);
            }

            diesel::update(
                o::app_delivery_orders
                    .filter(o::id.eq(order_id))
                    .filter(o::driver_id.eq(driver_uuid))
                    // A cancelled order stays cancelled.
                    .filter(o::status.eq(delivery_status("arriving_pickup")))
            )
            .set((
                o::driver_id.eq(None::<Uuid>),
                o::status.eq(delivery_status("pending")),
                o::delivery_accepted_time.eq(None::<DateTime<Utc>>),
                o::modified_at.eq(diesel::dsl::now),
            ))
            .execute(conn)?;

            refresh_driver_status(conn, driver_uuid)?;
            Ok(AssignAttempt::NotStarted)
        })
        .map_err(|e: diesel::result::Error| e.to_string())
    }).await
}

// Waits for the driver's wake-up signal or the end of their window, whichever comes first.
async fn wait_for_driver(rx: oneshot::Receiver<()>, window_secs: u64) {
    let window_ends = Instant::now() + Duration::from_secs(window_secs);
    if let Ok(Err(_)) = timeout(Duration::from_secs(window_secs), rx).await {
        // The sender was dropped without an answer (replaced by another search), so
        // there's no early signal coming; wait out the window instead.
        sleep_until(window_ends).await;
    }
}

// Background driver search for an order. Works through the closest drivers one at a time in
// the order's dispatch mode until one takes it or the search window closes, in which case the
// order is marked no_driver.
// `resume`: seconds this order has already searched (a search picked up by the sweep), which
// shortens the window and keeps the radius it had reached.
pub async fn run_assign_driver(pool: web::Data<DbPool>, order_id: i64, resume: Option<u64>) {
    let already = resume.unwrap_or(0);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(SEARCH_WINDOW_SECS.saturating_sub(already));
    let searched_secs = move || already + started.elapsed().as_secs();

    let input = match with_conn(&pool, move |conn| load_search_input(conn, order_id, resume.is_none())).await {
        Ok(input) => {
            println!("Order {}: searching within {}km of the pickup", order_id, search_radius_km(input.attempt, already));
            std::sync::Arc::new(input)
        }
        Err(e) => {
            eprintln!("Order {}: cannot start driver search: {}", order_id, e);
            return;
        }
    };
    let window = Duration::from_secs(match input.mode {
        DispatchMode::Offer => OFFER_TTL_SECS,
        DispatchMode::Assigned => ASSIGNED_START_SECS,
    });

    let mut radius_km = search_radius_km(input.attempt, already);

    'search: while Instant::now() < deadline {
        let widened = search_radius_km(input.attempt, searched_secs());
        if widened != radius_km {
            radius_km = widened;
            println!("Order {}: widening the search to {}km", order_id, radius_km);
        }

        match with_conn(&pool, move |conn| order_still_searching(conn, order_id)).await {
            Ok(true) => {},
            Ok(false) => {
                println!("Order {}: no longer pending, stopping search", order_id);
                return;
            }
            Err(e) => eprintln!("Order {}: status check failed: {}", order_id, e),
        }

        let candidates = {
            let input = input.clone();
            match with_conn(&pool, move |conn| load_candidates(conn, order_id, &input, radius_km)).await {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("Order {}: driver lookup failed: {}", order_id, e);
                    Vec::new()
                }
            }
        };

        for driver in candidates {
            if deadline.saturating_duration_since(Instant::now()) < window {
                break 'search;
            }
            let id = driver.driver_id;

            match input.mode {
                DispatchMode::Offer => match offer_to_driver(&pool, order_id, id).await {
                    Ok(true) => {
                        println!("Order {}: accepted by driver {}", order_id, id);
                        return;
                    }
                    Ok(false) => println!("Order {}: driver {} did not take it", order_id, id),
                    Err(e) => eprintln!("Order {}: offer to driver {} failed: {}", order_id, id, e),
                },
                DispatchMode::Assigned => match assign_to_driver(&pool, order_id, id).await {
                    Ok(AssignAttempt::Started) => {
                        println!("Order {}: started by assigned driver {}", order_id, id);
                        return;
                    }
                    Ok(AssignAttempt::NotStarted) => println!("Order {}: driver {} never started, reassigning", order_id, id),
                    Ok(AssignAttempt::Skipped(AssignOutcome::OrderTaken)) => {
                        println!("Order {}: no longer assignable, stopping search", order_id);
                        return;
                    }
                    Ok(AssignAttempt::Skipped(_)) => println!("Order {}: driver {} no longer free", order_id, id),
                    Err(e) => eprintln!("Order {}: assigning driver {} failed: {}", order_id, id, e),
                },
            }
        }

        sleep(Duration::from_secs(ROUND_PAUSE_SECS)).await;
    }

    let marked = with_conn(&pool, move |conn| {
        use crate::schema::app_delivery_orders::dsl as o;
        diesel::update(
            o::app_delivery_orders
                .filter(o::id.eq(order_id))
                .filter(o::driver_id.is_null())
                .filter(o::status.eq(delivery_status("pending")))
        )
        .set((
            o::status.eq(delivery_status("no_driver")),
            o::modified_at.eq(diesel::dsl::now),
        ))
        .execute(conn)
        .map_err(|e| e.to_string())
    }).await;

    match marked {
        Ok(1) => {
            println!("Order {}: no driver found, marked no_driver", order_id);
            zazu::spawn_notify_status(pool.clone(), order_id);
        }
        Ok(_) => println!("Order {}: search ended, order already moved on", order_id),
        Err(e) => eprintln!("Order {}: failed to mark no_driver: {}", order_id, e),
    }
}


pub fn validate_rider_account(
    connection: &mut PgConnection,
    rider_id: Uuid
) -> Result<Rider, String> {
    // No `*` import here: it would bring in the `rider_id` column and shadow the parameter.
    use crate::schema::back_custom_users::dsl::back_custom_users as riders;

    let rider: Rider = riders
        .find(rider_id)
        .select(Rider::as_select())
        .first::<Rider>(connection)
        .map_err(|_| "Rider not found".to_string())?;

    Ok(rider)
}

// Price and time for one ride type. Quoted before payment; the checkout charges this price.
// The price for a route, ride type and package. Every place that prices a trip (quote screen,
// checkout, ride request, the stored request) calls this with the same items, so they agree.
pub fn quote(pick_up: &GeoPoint, drop_off: &GeoPoint, ride_type: &RideType, items: &[ItemDetails]) -> RideQuote {
    let distance_km = distance_between(pick_up, drop_off);
    let estimated_time_min = pricing::estimated_time_min(distance_km, ride_type);
    let base = match ride_type {
        RideType::ASAP => pricing::calculate_asap(distance_km, estimated_time_min),
        RideType::ASAPEXPRESS => pricing::calculate_express(distance_km, estimated_time_min),
    };
    let insurance = pricing::insurance_premium(items);
    RideQuote {
        ride_type: ride_type.clone(),
        estimated_price: base + insurance,
        original_price: None,
        estimated_time_min,
        distance_km,
        insurance,
    }
}

// The app's quote screen: price and time for every ride type, used to build the ride request.
pub async fn ride_quote(
    _auth: AnyAuthGuard,
    pool: web::Data<DbPool>,
    body: web::Json<QuoteRequest>,
) -> HttpResponse {
    let QuoteRequest { pick_up, drop_off, items } = body.into_inner();
    if let Err(msg) = validate_points(&pick_up, &drop_off).and_then(|_| validate_items(&items)) {
        return json_error(StatusCode::BAD_REQUEST, msg);
    }
    let (pick, drop) = (pick_up.clone(), drop_off.clone());
    if let Err(e) = blocking(&pool, move |conn| require_service_area(conn, &pick, &drop)).await {
        return e.response();
    }
    let quotes: Vec<RideQuote> = [RideType::ASAP, RideType::ASAPEXPRESS]
        .iter()
        .map(|ride_type| quote(&pick_up, &drop_off, ride_type, &items))
        .collect();
    HttpResponse::Ok().json(quotes)
}

// Stores the request for this order unless one is already on file, and returns its quote.
fn ensure_ride_request(conn: &mut PgConnection, req: &CreateRideRequest, rider_id: Uuid, order_ref: Uuid) -> Result<(i64, i32), ApiError> {
    use crate::schema::back_ride_request::dsl::back_ride_request as ride_request;

    if let Some(existing) = find_ride_request(conn, order_ref)? {
        return Ok((existing.estimated_price, existing.estimated_time_min));
    }
    let new_request = NewRideRequest::new(req, rider_id, order_ref).map_err(ApiError::Internal)?;
    diesel::insert_into(ride_request)
        .values(&new_request)
        .execute(conn)?;
    Ok((new_request.estimated_price, new_request.estimated_time_min))
}

// Payment confirmed: awaiting_payment -> pending. Guarded, so a repeat changes nothing.
pub fn open_paid_order(conn: &mut PgConnection, order_id: i64) -> QueryResult<bool> {
    use crate::schema::app_delivery_orders::dsl as o;

    let opened = diesel::update(
        o::app_delivery_orders
            .filter(o::id.eq(order_id))
            .filter(o::status.eq(delivery_status("awaiting_payment")))
    )
    .set((
        o::status.eq(delivery_status("pending")),
        o::modified_at.eq(diesel::dsl::now),
    ))
    .execute(conn)?;
    Ok(opened == 1)
}

// Records the ride request against its delivery order and starts the driver search in the
// background, responding straight away; the outcome lands on app_delivery_orders
// (arriving_pickup or no_driver).
//   app orders (signed-in customer): the order was created by create_order and paid through
//     /payments/korapay/initialize; order_ref finds it. A repeat only starts a new search when
//     the last one ended in no_driver (/riders/assign-driver does the same).
//   Zazu orders (x-asap-secret): no payment here; the order is created or set to pending
//     and the search starts.
pub async fn request_ride(
    caller: Caller,
    pool: web::Data<DbPool>,
    body: web::Json<CreateRideRequest>,
) -> HttpResponse {
    let mut req = body.into_inner();
    if let Err(msg) = req.validate() {
        return json_error(StatusCode::BAD_REQUEST, msg);
    }
    // App and Zazu orders alike.
    let (pick, drop) = (req.pick_up.clone(), req.drop_off.clone());
    if let Err(e) = blocking(&pool, move |conn| require_service_area(conn, &pick, &drop)).await {
        return e.response();
    }

    let source = req.source.unwrap_or(match caller {
        Caller::Service => OrderSource::Zazu,
        Caller::User(_) => OrderSource::App,
    });

    let result = match (caller, source) {
        (Caller::User(user_id), OrderSource::App) => app_ride_request(&pool, user_id, req).await,
        (Caller::Service, OrderSource::Zazu) => zazu_ride_request(&pool, req).await,
        (Caller::User(_), OrderSource::Zazu) =>
            Err(ApiError::Forbidden("Zazu orders can only be placed by the Zazu service".into())),
        (Caller::Service, OrderSource::App) =>
            Err(ApiError::BadRequest("App orders are created through /payments/korapay/initialize".into())),
    };

    match result {
        Ok((order_id, price, minutes)) => search_started(order_id, price, minutes),
        Err(e) => e.response(),
    }
}

// Points closer than this are the same place (GPS noise between checkout and request).
const SAME_PLACE_KM: f64 = 0.1;

pub fn same_place(stored: Option<GeoPoint>, sent: &GeoPoint) -> bool {
    stored.map_or(false, |stored| distance_between(&stored, sent) <= SAME_PLACE_KM)
}

// A ride request for a cancelled order. The checkout can't be closed from here, so a payment
// can still land after the order was cancelled (left open past a cancel or the sweep). With no
// rider ever assigned, whatever Korapay took goes back; the order stays cancelled either way.
async fn cancelled_order(order: &OrderRow) -> ApiError {
    let reference = match (order.driver_id, order.payment_reference.as_deref()) {
        (None, Some(reference)) => reference,
        _ => return ApiError::Conflict("Order was cancelled".into()),
    };
    match korapay::refund_if_paid(reference, "Your order was cancelled before a rider was assigned").await {
        Ok(RefundOutcome::Initiated(amount)) =>
            ApiError::Conflict(format!("Order was cancelled; your payment of {} is being refunded", amount)),
        Ok(RefundOutcome::AlreadyRefunded) =>
            ApiError::Conflict("Order was cancelled; your payment has been refunded".into()),
        Ok(RefundOutcome::NothingPaid) => ApiError::Conflict("Order was cancelled".into()),
        Err(e) => {
            // The sweep retries the refund.
            eprintln!("Order {}: cancelled, late-payment refund check failed: {}", order.id, e);
            ApiError::Conflict("Order was cancelled; any payment will be refunded".into())
        }
    }
}

// The app order was created by create_order and paid through /payments/korapay/initialize;
// order_ref finds it. Korapay is asked whether the money arrived before the order opens.
async fn app_ride_request(pool: &web::Data<DbPool>, user_id: Uuid, req: CreateRideRequest) -> Result<(i64, i64, i32), ApiError> {
    let order_ref = req.order_ref.ok_or_else(|| ApiError::BadRequest(
        "order_ref is required for app orders".into()
    ))?;
    if req.rider_id.map_or(false, |id| id != user_id) {
        return Err(ApiError::Forbidden("rider_id does not match the signed-in user".into()));
    }

    let order = blocking(pool, move |conn| Ok(load_order(conn, OrderKey::Ref(order_ref), false)?)).await?
        .filter(|order| order.client_id == user_id && order.source() == OrderSource::App)
        .ok_or_else(|| ApiError::NotFound("No order for this order_ref".into()))?;

    match order.status.as_str() {
        "awaiting_payment" | "pending" | "no_driver" => {},
        "cancelled" => return Err(cancelled_order(&order).await),
        _ => return Err(ApiError::Conflict("Order already has a driver or is finished".into())),
    }
    if !same_place(order.pick_up(), &req.pick_up) || !same_place(order.drop_off(), &req.drop_off) {
        return Err(ApiError::Conflict("pick_up or drop_off differs from the paid order".into()));
    }

    // The money must have arrived and cover this request's price (ride type and distance).
    let price = quote(&req.pick_up, &req.drop_off, &req.ride_type, &req.items).estimated_price;
    paid_app_order(&order, price).await?;

    let order_id = order.id;
    let (price, minutes) = blocking(pool, move |conn| conn.transaction(|conn| {
        let order = load_order(conn, OrderKey::Id(order_id), true)?
            .ok_or_else(|| ApiError::NotFound("Order not found".into()))?;
        match order.status.as_str() {
            "awaiting_payment" => { open_paid_order(conn, order.id)?; },
            "pending" => {},
            // The last search found nobody: the same paid reference may search again.
            "no_driver" => reopen_order(conn, order.id)?,
            "cancelled" => return Err(ApiError::Conflict("Order was cancelled".into())),
            _ => return Err(ApiError::Conflict("Order already has a driver or is finished".into())),
        }
        let order_ref = order.order_ref.ok_or_else(|| ApiError::Internal("order has no order_ref".into()))?;
        ensure_ride_request(conn, &req, user_id, order_ref)
    })).await?;

    start_search(pool.clone(), order_id);
    Ok((order_id, price, minutes))
}

async fn zazu_ride_request(pool: &web::Data<DbPool>, req: CreateRideRequest) -> Result<(i64, i64, i32), ApiError> {
    let order_ref = req.order_ref.ok_or_else(|| ApiError::BadRequest("order_ref is required for Zazu orders".into()))?;
    let rider_id = req.rider_id.ok_or_else(|| ApiError::BadRequest("rider_id is required for Zazu orders".into()))?;
    let customer_phone = req.customer_phone.clone().ok_or_else(|| ApiError::BadRequest("customer_phone is required".into()))?;

    let (order_id, price, minutes) = blocking(pool, move |conn| conn.transaction(|conn| {
        validate_rider_account(conn, rider_id)
            .map_err(|e| ApiError::BadRequest(format!("rider validation error: {}", e)))?;

        let order_id = match find_order_id(conn, order_ref)? {
            Some(order_id) => {
                reopen_order(conn, order_id)?;
                ensure_codes(conn, order_id)?;
                order_id
            }
            None => insert_order(conn, &NewOrder {
                client_id: rider_id,
                status: "pending",
                source: OrderSource::Zazu,
                order_ref,
                payment_reference: req.payment_reference.as_deref(),
                customer_phone: &customer_phone,
                request: &req,
            })?,
        };

        let (price, minutes) = ensure_ride_request(conn, &req, rider_id, order_ref)?;
        Ok((order_id, price, minutes))
    })).await?;

    start_search(pool.clone(), order_id);
    Ok((order_id, price, minutes))
}

pub fn routes() -> Scope {
    web::scope("/riders")
        .route("/assign-driver", web::post().to(assign_driver_handler))
        .route("/ride-request", web::post().to(request_ride))
        .route("/ride-quote", web::post().to(ride_quote))
        .route("/profile", web::post().to(crate::api::profile::update_profile))
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub enum RideType {
    ASAP,
    ASAPEXPRESS,
}

impl RideType {
    pub fn vehicle_types(&self) -> Vec<String> {
        match self {
            RideType::ASAP => vec!["Bike".to_string()],
            RideType::ASAPEXPRESS => vec!["EV".to_string()],
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum OrderSource {
    App,
    Zazu,
}

impl OrderSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderSource::App => "app",
            OrderSource::Zazu => "zazu",
        }
    }
}

#[derive(Deserialize)]
pub struct QuoteRequest {
    pub pick_up: GeoPoint,
    pub drop_off: GeoPoint,
    // Empty for the first quote (route only); sent again with the items once they're entered,
    // so the price is the package's.
    #[serde(default)]
    pub items: Vec<ItemDetails>,
}

// Matches the app's RideQuote type.
#[derive(Serialize)]
pub struct RideQuote {
    pub ride_type: RideType,
    pub estimated_price: i64,
    // Set when a discount applies.
    pub original_price: Option<i64>,
    pub estimated_time_min: i32,
    pub distance_km: f64,
    // Naira, already inside estimated_price: the package insurance (pricing::insurance_premium).
    // 0 for the route-only quote.
    pub insurance: i64,
}

#[derive(Deserialize)]
pub struct CreateRideRequest {
    // Defaults to the caller: app for a signed-in user, zazu for the Zazu service.
    #[serde(default)]
    pub source: Option<OrderSource>,
    // The customer. App orders take it from the token (a different value is rejected).
    #[serde(default)]
    pub rider_id: Option<Uuid>,
    pub pick_up: GeoPoint,
    pub drop_off: GeoPoint,
    pub ride_type: RideType,
    #[serde(default)]
    pub payment_method: String,
    pub items: Vec<ItemDetails>,
    // Links the request to its app_delivery_orders row. Required: Zazu sends its own, the app
    // sends the one create_order returned.
    #[serde(default)]
    pub order_ref: Option<Uuid>,
    #[serde(default)]
    pub user_id: Option<i64>,
    #[serde(default)]
    pub user_phone_number: Option<String>,
    #[serde(default)]
    pub vendor_phone_number: Option<String>,
    // Required: Zazu sends it; app orders fill it from the customer's profile.
    #[serde(default)]
    pub customer_phone: Option<String>,
    #[serde(default)]
    pub pickup_contact_name: Option<String>,
    #[serde(default)]
    pub pickup_contact_phone: Option<String>,
    #[serde(default)]
    pub pickup_name: Option<String>,
    #[serde(default)]
    pub dropoff_name: Option<String>,
    // Zazu's own payment reference, stored as sent. App orders read theirs from the order row.
    #[serde(default)]
    pub payment_reference: Option<String>,
}

// Delivery limits per request.
const MAX_TOTAL_ITEMS: u32 = 10;
const MAX_TOTAL_WEIGHT_KG: f64 = 5.0;
// Longest side first: what fits a delivery box. Checked side by side after sorting the
// item's sides, so the orientation it was measured in doesn't matter.
const MAX_ITEM_SIDES_CM: [f64; 3] = [60.0, 45.0, 45.0];
// Shorter than this, pickup and drop-off are the same place.
const MIN_TRIP_KM: f64 = 0.05;

fn valid_point(point: &GeoPoint) -> bool {
    point.lat.is_finite() && point.lng.is_finite()
        && (-90.0..=90.0).contains(&point.lat)
        && (-180.0..=180.0).contains(&point.lng)
        // (0, 0) is what an unset location serialises to.
        && !(point.lat == 0.0 && point.lng == 0.0)
}

pub fn validate_points(pick_up: &GeoPoint, drop_off: &GeoPoint) -> Result<(), String> {
    if !valid_point(pick_up) {
        return Err("pick_up is not a valid location".into());
    }
    if !valid_point(drop_off) {
        return Err("drop_off is not a valid location".into());
    }
    if distance_between(pick_up, drop_off) < MIN_TRIP_KM {
        return Err("pick_up and drop_off are the same place".into());
    }
    Ok(())
}

fn trimmed(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

// Empty means absent; anything else must parse as a phone number, stored as E.164.
fn normalize_optional_phone(field: &str, value: Option<String>) -> Result<Option<String>, String> {
    match trimmed(value) {
        None => Ok(None),
        Some(raw) => normalize_phone(&raw)
            .map(Some)
            .ok_or_else(|| format!("{} is not a valid phone number", field)),
    }
}

// Each item, then the whole load: the quote checks the same as the ride request, so a quoted
// package is one that can be booked.
pub fn validate_items(items: &[ItemDetails]) -> Result<(), String> {
    for item in items {
        item.validate()?;
    }
    let total_items: u32 = items.iter().map(|item| item.quantity).sum();
    if total_items > MAX_TOTAL_ITEMS {
        return Err(format!("Too many items: {} (at most {})", total_items, MAX_TOTAL_ITEMS));
    }
    let total_weight: f64 = items.iter().map(|item| item.weight * item.quantity as f64).sum();
    if total_weight > MAX_TOTAL_WEIGHT_KG {
        return Err(format!("Too heavy: {:.1}kg in total (at most {}kg)", total_weight, MAX_TOTAL_WEIGHT_KG));
    }
    Ok(())
}

impl CreateRideRequest {
    // Shape checks that need no DB. Normalises phone numbers to E.164 and blank strings to None.
    pub fn validate(&mut self) -> Result<(), String> {
        validate_points(&self.pick_up, &self.drop_off)?;

        if self.items.is_empty() {
            return Err("items must list at least one item".into());
        }
        validate_items(&self.items)?;

        self.customer_phone = normalize_optional_phone("customer_phone", self.customer_phone.take())?;
        self.pickup_contact_phone = normalize_optional_phone("pickup_contact_phone", self.pickup_contact_phone.take())?;
        self.user_phone_number = normalize_optional_phone("user_phone_number", self.user_phone_number.take())?;
        self.vendor_phone_number = normalize_optional_phone("vendor_phone_number", self.vendor_phone_number.take())?;

        self.pickup_contact_name = trimmed(self.pickup_contact_name.take());
        self.pickup_name = trimmed(self.pickup_name.take());
        self.dropoff_name = trimmed(self.dropoff_name.take());
        self.payment_reference = trimmed(self.payment_reference.take());
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Insertable, Clone)]
#[diesel(table_name = crate::schema::back_ride_request)]
pub struct NewRideRequest {
    pub request_id: Uuid,
    pub rider_id: Uuid,
    pub pick_up: serde_json::Value,
    pub drop_off: serde_json::Value,
    pub estimated_price: i64,
    pub distance_km: f64,
    pub estimated_time_min: i32,
    pub ride_type: serde_json::Value,
    pub items: serde_json::Value,
    pub payment_method: String,
    pub order_ref: Option<Uuid>,
    pub user_id: Option<i64>,
    pub user_phone_number: Option<String>,
    pub vendor_phone_number: Option<String>,
}


impl NewRideRequest {
    pub fn new(req: &CreateRideRequest, rider_id: Uuid, order_ref: Uuid) -> Result<Self, String> {
        let quote = quote(&req.pick_up, &req.drop_off, &req.ride_type, &req.items);
        let json = |value: Result<serde_json::Value, serde_json::Error>, field: &str| {
            value.map_err(|e| format!("could not store {}: {}", field, e))
        };

        Ok(Self {
            request_id: Uuid::new_v4(),
            rider_id,
            pick_up: json(serde_json::to_value(&req.pick_up), "pick_up")?,
            drop_off: json(serde_json::to_value(&req.drop_off), "drop_off")?,
            ride_type: json(serde_json::to_value(&req.ride_type), "ride_type")?,
            items: json(serde_json::to_value(&req.items), "items")?,
            estimated_price: quote.estimated_price,
            distance_km: quote.distance_km,
            estimated_time_min: quote.estimated_time_min,
            payment_method: req.payment_method.clone(),
            order_ref: Some(order_ref),
            user_id: req.user_id,
            user_phone_number: req.user_phone_number.clone(),
            vendor_phone_number: req.vendor_phone_number.clone(),
        })
    }
}


#[derive(Serialize, Deserialize, Clone, Queryable, Selectable)]
#[diesel(table_name = crate::schema::back_ride_request)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct RideRequest {
    pub request_id: Uuid,
    pub rider_id: Uuid,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub pick_up: serde_json::Value,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub drop_off: serde_json::Value,
    pub estimated_price: i64,
    pub distance_km: f64,
    pub estimated_time_min: i32,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub ride_type: serde_json::Value,
    pub items: serde_json::Value,
    pub payment_method: String,
    pub order_ref: Option<Uuid>, //from zazu
    pub user_id: Option<i64>,
    pub user_phone_number: Option<String>,
    pub vendor_phone_number: Option<String>,
}


#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ItemDetails {
    pub name: String,
    // Naira, the declared value of the whole line (not per unit); the app may send decimals.
    pub price: f64,
    pub dimensions: (f64, f64, f64), // (length, width, height) in cm
    pub quantity: u32,
    pub weight: f64, // in kg, per unit
    #[serde(default)]
    pub image_url: Option<String>,
    // The package type from the app's item type screen; `type` on the wire. Prices the trip
    // (pricing::package_type_factor). None before the customer picks one.
    #[serde(default, rename = "type")]
    pub package_type: Option<String>,
}

impl ItemDetails {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Every item needs a name".into());
        }
        if !self.price.is_finite() || self.price < 0.0 {
            return Err(format!("'{}' has an invalid price", self.name));
        }
        if self.package_type.as_deref().map_or(false, pricing::is_blocked_type) {
            return Err(format!("'{}': hazardous or restricted items can't be delivered", self.name));
        }
        self.max_dimensions()?;
        self.aggregate_quantity()?;
        self.max_weight()
    }

    // Compares each side against its limit (longest to longest), not the tuple as a whole:
    // tuple comparison only really looked at the length.
    pub fn max_dimensions(&self) -> Result<(), String> {
        let (length, width, height) = self.dimensions;
        let mut sides = [length, width, height];
        if sides.iter().any(|side| !side.is_finite() || *side <= 0.0) {
            return Err(format!("'{}' needs a positive length, width and height in cm", self.name));
        }
        sides.sort_by(|a, b| b.total_cmp(a));
        if sides.iter().zip(MAX_ITEM_SIDES_CM.iter()).any(|(side, limit)| side > limit) {
            return Err(format!(
                "Sorry, we can't handle '{}' at this size (at most {}x{}x{}cm)",
                self.name, MAX_ITEM_SIDES_CM[0], MAX_ITEM_SIDES_CM[1], MAX_ITEM_SIDES_CM[2]
            ));
        }
        Ok(())
    }

    pub fn aggregate_quantity(&self) -> Result<(), String> {
        if self.quantity == 0 {
            return Err(format!("'{}' needs a quantity of at least 1", self.name));
        }
        if self.quantity > MAX_TOTAL_ITEMS {
            return Err(format!("Too many of '{}' (at most {})", self.name, MAX_TOTAL_ITEMS));
        }
        Ok(())
    }

    pub fn max_weight(&self) -> Result<(), String> {
        if !self.weight.is_finite() || self.weight <= 0.0 {
            return Err(format!("'{}' needs a positive weight in kg", self.name));
        }
        if self.weight * self.quantity as f64 > MAX_TOTAL_WEIGHT_KG {
            return Err(format!("Sorry, '{}' is too heavy (at most {}kg)", self.name, MAX_TOTAL_WEIGHT_KG));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(dimensions: (f64, f64, f64)) -> ItemDetails {
        ItemDetails { name: "Box".into(), price: 1000.0, dimensions, quantity: 1, weight: 1.0, image_url: None, package_type: None }
    }

    #[test]
    fn size_check_compares_every_side() {
        assert!(item((40.0, 30.0, 20.0)).max_dimensions().is_ok());
        // Same box measured in another orientation.
        assert!(item((20.0, 40.0, 30.0)).max_dimensions().is_ok());
        // Short length but one side too wide: the old tuple comparison let this through.
        assert!(item((10.0, 80.0, 10.0)).max_dimensions().is_err());
        assert!(item((50.0, 50.0, 50.0)).max_dimensions().is_err());
        assert!(item((0.0, 10.0, 10.0)).max_dimensions().is_err());
    }

    #[test]
    fn search_radius_widens_per_attempt() {
        assert_eq!(search_radius_km(0, 0), 5.0);
        assert_eq!(search_radius_km(1, 0), 5.0);
        assert_eq!(search_radius_km(2, 0), 8.0);
        assert_eq!(search_radius_km(3, 0), 12.0);
        assert_eq!(search_radius_km(9, 0), 12.0);
    }

    #[test]
    fn search_radius_widens_every_three_minutes_up_to_twelve() {
        assert_eq!(search_radius_km(1, 179), 5.0);
        assert_eq!(search_radius_km(1, 180), 8.0);
        assert_eq!(search_radius_km(1, 360), 12.0);
        assert_eq!(search_radius_km(1, 1800), 12.0);
        // A Try again starts wider and still caps at 12.
        assert_eq!(search_radius_km(2, 180), 12.0);
    }

    #[test]
    fn rejects_bad_points() {
        let lagos = GeoPoint { lat: 6.5244, lng: 3.3792, name: None };
        let ikeja = GeoPoint { lat: 6.6018, lng: 3.3515, name: None };
        assert!(validate_points(&lagos, &ikeja).is_ok());
        assert!(validate_points(&lagos, &lagos).is_err());
        assert!(validate_points(&GeoPoint { lat: 0.0, lng: 0.0, name: None }, &ikeja).is_err());
        assert!(validate_points(&GeoPoint { lat: 95.0, lng: 3.0, name: None }, &ikeja).is_err());
        assert!(validate_points(&GeoPoint { lat: f64::NAN, lng: 3.0, name: None }, &ikeja).is_err());
    }
}
