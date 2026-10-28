use actix_web::{ web, Scope, HttpResponse, http::StatusCode };
use diesel::prelude::*;
use diesel::sql_types::{ BigInt, Double, Uuid as SqlUuid };
use diesel::pg::PgConnection;
use uuid::Uuid;
use serde::{ Deserialize, Serialize };
use std::collections::HashSet;
use std::sync::Mutex;
use std::env;
use reqwest::Client;
use lazy_static::lazy_static;
use crate::services::pricing::{GeoPoint, distance_between};
use crate::services::auth::AuthUser;
use crate::schema::back_drivers::dsl::{back_drivers as drivers, *};
use crate::schema::back_ride_request::dsl::{back_ride_request as ride_request, *};
use crate::api::errors::{ ApiError, blocking, json_error };
use crate::db::DbPool;

const ARRIVAL_THRESHOLD_KM: f64 = 0.15; // 150 metres

lazy_static! {
    // Zazu orders already reported as at the drop-off, so later location updates don't repeat it.
    // In memory: after a restart an arrival may be reported once more.
    static ref ARRIVAL_SENT: Mutex<HashSet<i64>> = Mutex::new(HashSet::new());
}
const ARRIVAL_SENT_CAP: usize = 10_000;

struct ZazuArrivalPayload {
    order_ref: Uuid,
    driver_lat: f64,
    driver_lng: f64,
}


// The path id is only used for pick_up/drop_off, where it is the ride request's id and the
// request must belong to the signed-in rider. A driver location always updates the signed-in
// driver; the path id is ignored there.
pub async fn process_geolocation(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    path: web::Path<Uuid>,
    payload: web::Json<GeoPointRequest>,
) -> HttpResponse {
    let me = auth.user_id;
    let id = path.into_inner();
    let gp = GeoPoint::new(payload.lat, payload.lng, payload.name.clone());
    let kind = payload.kind.clone();
    let (new_lat, new_lng) = (payload.lat, payload.lng);

    let result = blocking(&pool, move |conn| -> Result<(usize, Vec<ZazuArrivalPayload>), ApiError> {
        match kind {
            GeoPointKind::DriverLocation => {
                let rows = update_driver_location(conn, me, &gp)?;
                if rows > 0 {
                    crate::services::sessions::record_ping(conn, me);
                }
                Ok((rows, check_dropoff_arrival(conn, me, new_lat, new_lng)?))
            }
            GeoPointKind::PickUp => Ok((update_request_pickup(conn, id, me, &gp)?, Vec::new())),
            GeoPointKind::DropOff => Ok((update_request_dropoff(conn, id, me, &gp)?, Vec::new())),
        }
    }).await;

    match result {
        Ok((rows, arrivals)) if rows > 0 => {
            for info in arrivals {
                tokio::spawn(notify_zazu_driver_arrived(info));
            }
            HttpResponse::Ok().json(serde_json::json!({ "status": "ok" }))
        }
        Ok(_) => json_error(StatusCode::NOT_FOUND, "Row not found"),
        Err(e) => e.response(),
    }
}

#[derive(QueryableByName)]
struct InTransitZazuOrder {
    #[diesel(sql_type = BigInt)]
    id: i64,
    // Not `order_ref`: the back_ride_request glob import above makes that name a column struct.
    #[diesel(sql_type = SqlUuid)]
    zazu_order_ref: Uuid,
    #[diesel(sql_type = Double)]
    dropoff_lat: f64,
    #[diesel(sql_type = Double)]
    dropoff_long: f64,
}

// The driver's in-transit Zazu orders whose drop-off they just reached, each reported once.
fn check_dropoff_arrival(
    conn: &mut PgConnection,
    driver_id_val: Uuid,
    lat: f64,
    lng: f64,
) -> QueryResult<Vec<ZazuArrivalPayload>> {
    let orders: Vec<InTransitZazuOrder> = diesel::sql_query(
        "SELECT id, order_ref AS zazu_order_ref, dropoff_lat::float8 AS dropoff_lat, dropoff_long::float8 AS dropoff_long \
         FROM app_delivery_orders \
         WHERE driver_id = $1 AND status = 'in_transit' AND lower(source) = 'zazu' \
           AND order_ref IS NOT NULL AND dropoff_lat IS NOT NULL AND dropoff_long IS NOT NULL"
    )
    .bind::<SqlUuid, _>(driver_id_val)
    .load(conn)?;

    let here = GeoPoint { lat, lng, name: None };
    let mut sent = ARRIVAL_SENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if sent.len() > ARRIVAL_SENT_CAP {
        sent.clear();
    }

    Ok(orders.into_iter()
        .filter(|order| {
            let dropoff = GeoPoint { lat: order.dropoff_lat, lng: order.dropoff_long, name: None };
            distance_between(&here, &dropoff) <= ARRIVAL_THRESHOLD_KM
        })
        .filter(|order| sent.insert(order.id))
        .map(|order| ZazuArrivalPayload { order_ref: order.zazu_order_ref, driver_lat: lat, driver_lng: lng })
        .collect())
}


async fn notify_zazu_driver_arrived(info: ZazuArrivalPayload) {
    use crate::api::drivers::ZAZU_URL;

    let zazu_url = ZAZU_URL.lock().await.clone();
    if zazu_url.is_empty() {
        eprintln!("Zazu URL not configured — driver arrival not sent for {}", info.order_ref);
        return;
    }

    let secret = env::var("ASAP_WEBHOOK_SECRET").unwrap_or_default();
    let payload = serde_json::json!({
        "order_ref": info.order_ref,
        "driver_lat": info.driver_lat,
        "driver_lng": info.driver_lng,
    });

    match Client::new()
        .post(format!("{}/driver-arrived", zazu_url))
        .header("x-asap-secret", &secret)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => println!("Zazu driver-arrived sent: {}", resp.status()),
        Err(e) => eprintln!("Failed to notify Zazu driver-arrived: {}", e),
    }
}


fn to_json(gp: &GeoPoint) -> QueryResult<serde_json::Value> {
    serde_json::to_value(gp).map_err(|e| diesel::result::Error::SerializationError(Box::new(e)))
}

pub fn update_driver_location(
    conn: &mut PgConnection,
    driver_id_val: Uuid,
    gp: &GeoPoint,
) -> QueryResult<usize> {
    diesel::update(drivers.filter(driver_id.eq(&driver_id_val)))
        .set((
            DriverUpdateLocation { driver_location: to_json(gp)? },
            location_updated_at.eq(crate::api::riders::db_now_nullable()),
        ))
        .execute(conn)
}

// Only the rider's own request; someone else's request id updates nothing (404).
pub fn update_request_pickup(
    conn: &mut PgConnection,
    other_request_id: Uuid,
    rider: Uuid,
    gp: &GeoPoint,
) -> QueryResult<usize> {
    use crate::schema::back_ride_request::dsl::rider_id as request_rider_id;
    diesel::update(ride_request.filter(request_id.eq(&other_request_id)).filter(request_rider_id.eq(rider)))
        .set(RideRequestUpdateLocation { pick_up: to_json(gp)? })
        .execute(conn)
}

pub fn update_request_dropoff(
    conn: &mut PgConnection,
    other_request_id: Uuid,
    rider: Uuid,
    gp: &GeoPoint,
) -> QueryResult<usize> {
    use crate::schema::back_ride_request::dsl::rider_id as request_rider_id;
    diesel::update(ride_request.filter(request_id.eq(&other_request_id)).filter(request_rider_id.eq(rider)))
        .set(RideRequestUpdateDropOffLocation { drop_off: to_json(gp)? })
        .execute(conn)
}


#[derive(Deserialize)]
pub struct GeoPointRequest {
    pub lat: f64,
    pub lng: f64,
    pub name: String,
    pub kind: GeoPointKind,
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum GeoPointKind {
    DriverLocation,
    PickUp,
    DropOff,
}

#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::back_drivers)]
pub struct DriverUpdateLocation {
    #[diesel(sql_type = Jsonb)]
    pub driver_location: serde_json::Value,
}

#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::back_ride_request)]
pub struct RideRequestUpdateLocation {
    #[diesel(sql_type = Jsonb)]
    pub pick_up: serde_json::Value,
}

#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::back_ride_request)]
pub struct RideRequestUpdateDropOffLocation {
    #[diesel(sql_type = Jsonb)]
    pub drop_off: serde_json::Value,
}


pub fn routes() -> Scope {
    web::scope("/matching")
        .route("/process-geolocation/{id}", web::post().to(process_geolocation))
}
