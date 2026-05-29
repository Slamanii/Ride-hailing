use actix_web::{ web, Scope, HttpResponse };
use diesel::prelude::*;
use diesel::sql_types::Jsonb;
use diesel::pg::PgConnection;
use uuid::Uuid;
use serde_json::Value;
use serde::{ Deserialize, Serialize };
use crate::services::pricing::{GeoPoint, distance_between};
use crate::services::auth::AuthGuard;
use crate::schema::back_drivers::dsl::{back_drivers as drivers, *};
use crate::schema::back_ride_request::dsl::{back_ride_request as ride_request, *};
use crate::db::DbPool;
use std::env;
use reqwest::Client;

const ARRIVAL_THRESHOLD_KM: f64 = 0.15; // 150 metres

struct ZazuArrivalPayload {
    order_ref: Uuid,
    driver_lat: f64,
    driver_lng: f64,
}


pub async fn process_geolocation(
    _auth: AuthGuard,
    pool: web::Data<DbPool>,
    path: web::Path<Uuid>,
    payload: web::Json<GeoPointRequest>,
) -> HttpResponse {
    let id = path.into_inner();
    let gp = GeoPoint::new(payload.lat, payload.lng, payload.name.clone());
    let kind = payload.kind.clone();
    let new_lat = payload.lat;
    let new_lng = payload.lng;

    let result = web::block({
        let pool = pool.clone();
        move || -> Result<(usize, Option<ZazuArrivalPayload>), String> {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            match kind {
                GeoPointKind::DriverLocation => {
                    let rows = update_driver_location(&mut conn, id, &gp)
                        .map_err(|e| format!("{:?}", e))?;
                    let arrived = check_dropoff_arrival(&mut conn, id, new_lat, new_lng)?;
                    Ok((rows, arrived))
                }
                GeoPointKind::PickUp => {
                    let rows = update_request_pickup(&mut conn, id, &gp)
                        .map_err(|e| format!("{:?}", e))?;
                    Ok((rows, None))
                }
                GeoPointKind::DropOff => {
                    let rows = update_request_dropoff(&mut conn, id, &gp)
                        .map_err(|e| format!("{:?}", e))?;
                    Ok((rows, None))
                }
            }
        }
    }).await;

    match result {
        Ok(Ok((rows, arrived))) if rows > 0 => {
            if let Some(info) = arrived {
                tokio::spawn(async move {
                    notify_zazu_driver_arrived(info).await;
                });
            }
            HttpResponse::Ok().json(serde_json::json!({"status":"ok"}))
        }
        Ok(Ok(_)) => HttpResponse::NotFound().body("Row not found"),
        Ok(Err(e)) => HttpResponse::InternalServerError().body(format!("DB error: {:?}", e)),
        Err(e) => HttpResponse::InternalServerError().body(format!("Task error: {}", e)),
    }
}


fn check_dropoff_arrival(
    conn: &mut PgConnection,
    driver_id_val: Uuid,
    lat: f64,
    lng: f64,
) -> Result<Option<ZazuArrivalPayload>, String> {
    use crate::schema::back_trips::dsl as t;

    let active = t::back_trips
        .filter(t::driver_id.eq(driver_id_val))
        .filter(t::status.eq("Ongoing"))
        .select((t::drop_off, t::order_ref))
        .first::<(serde_json::Value, Option<Uuid>)>(conn)
        .optional()
        .map_err(|e| e.to_string())?;

    let (dropoff_val, order_ref_opt) = match active {
        Some(row) => row,
        None => return Ok(None),
    };

    let dropoff_gp: GeoPoint = serde_json::from_value(dropoff_val)
        .map_err(|e| format!("Invalid drop_off in trip: {}", e))?;

    let driver_gp = GeoPoint { lat, lng, name: None };

    if distance_between(&driver_gp, &dropoff_gp) > ARRIVAL_THRESHOLD_KM {
        return Ok(None);
    }

    // Mark driver as at dropoff to prevent re-firing on subsequent location updates
    diesel::update(
        t::back_trips
            .filter(t::driver_id.eq(driver_id_val))
            .filter(t::status.eq("Ongoing"))
    )
    .set(t::status.eq("driver_at_dropoff"))
    .execute(conn)
    .map_err(|e| e.to_string())?;

    // Only notify Zazu if this is a Zazu order
    match order_ref_opt {
        Some(ord) => Ok(Some(ZazuArrivalPayload { order_ref: ord, driver_lat: lat, driver_lng: lng })),
        None => Ok(None),
    }
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


pub fn update_driver_location(
    conn: &mut PgConnection,
    driver_id_val: Uuid,
    gp: &GeoPoint,
) -> QueryResult<usize> {
    let jsonl = serde_json::to_value(gp).expect("serialize GeoPoint");
    diesel::update(drivers.filter(driver_id.eq(&driver_id_val)))
        .set(DriverUpdateLocation { driver_location: jsonl })
        .execute(conn)
}

pub fn update_request_pickup(
    conn: &mut PgConnection,
    other_request_id: Uuid,
    gp: &GeoPoint,
) -> QueryResult<usize> {
    let jsonp = serde_json::to_value(gp).expect("serialize GeoPoint");
    diesel::update(ride_request.filter(request_id.eq(&other_request_id)))
        .set(RideRequestUpdateLocation { pick_up: jsonp })
        .execute(conn)
}

pub fn update_request_dropoff(
    conn: &mut PgConnection,
    other_request_id: Uuid,
    gp: &GeoPoint,
) -> QueryResult<usize> {
    let jsond = serde_json::to_value(gp).expect("serialize GeoPoint");
    diesel::update(ride_request.filter(request_id.eq(&other_request_id)))
        .set(RideRequestUpdateDropOffLocation { drop_off: jsond })
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
