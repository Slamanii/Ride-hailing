use actix_web::{ web, Scope, HttpResponse };
use diesel::prelude::*;
use diesel::sql_types::Jsonb;
use diesel::pg::PgConnection;
use uuid::Uuid;
use serde_json::Value;
use serde::{ Deserialize, Serialize };
use crate::services::pricing::GeoPoint;
use crate::schema::{ drivers::dsl::*, ride_request::dsl::* };
use crate::db::DbPool;




pub async fn process_geolocation(
    pool: web::Data<DbPool>,
    path: web::Path<Uuid>,             // id of driver or ride
    payload: web::Json<GeoPointRequest>,
) -> HttpResponse {
    let id = path.into_inner();
    let gp = GeoPoint::new(payload.lng, payload.lat, payload.name.clone());
    let kind = payload.kind.clone();

    let result = web::block({
        
        let pool = pool.clone();
        
        move || {
        let mut conn = pool.get().expect("Failed to get DB connection from pool");
        match kind {
            GeoPointKind::DriverLocation => {
                update_driver_location(&mut conn, id, &gp)
                    .map_err(|e| format!("{:?}", e))
            }
            GeoPointKind::PickUp => {
                update_request_pickup(&mut conn, id, &gp)
                    .map_err(|e| format!("{:?}", e))
            }
            GeoPointKind::DropOff => {
                update_request_dropoff(&mut conn, id, &gp)
                    .map_err(|e| format!("{:?}", e))
            }
        }
                }

    })
    .await;

    match result {
        Ok(Ok(rows)) if rows > 0 => HttpResponse::Ok().json(serde_json::json!({"status":"ok"})),
        Ok(Ok(_)) => HttpResponse::NotFound().body("Row not found"),
        Ok(Err(e)) => HttpResponse::InternalServerError().body(format!("DB error: {:?}", e)),
        Err(e) => HttpResponse::InternalServerError().body(format!("Task error: {}", e)),
    }
}


pub fn update_driver_location(
    conn: &mut PgConnection,
    driver_id_val: Uuid,
    gp: &GeoPoint,
) -> QueryResult<usize> {

    let jsonl: serde_json::Value = serde_json::to_value(gp).expect("Failed to serialize GeoPoint");  

    diesel::update(drivers.filter(driver_id.eq(&driver_id_val)))
        .set(DriverUpdateLocation {
        driver_location: jsonl,
    })
        .execute(conn)
}

pub fn update_request_pickup(
    conn: &mut PgConnection,
    other_request_id: Uuid,
    gp: &GeoPoint,
) -> QueryResult<usize> {

    let jsonp: serde_json::Value = serde_json::to_value(gp).expect("Failed to serialize GeoPoint");    

    diesel::update(ride_request.filter(request_id.eq(&other_request_id)))
        .set(RideRequestUpdateLocation {
        pick_up: jsonp,
    })
        .execute(conn)
}

pub fn update_request_dropoff(
    conn: &mut PgConnection,
    other_request_id: Uuid,
    gp: &GeoPoint,
) -> QueryResult<usize> {

    let jsond: serde_json::Value = serde_json::to_value(gp).expect("Failed to serialize GeoPoint");  

    diesel::update(ride_request.filter(request_id.eq(&other_request_id)))
        .set(RideRequestUpdateDropOffLocation {
        drop_off: jsond,
        })
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
#[diesel(table_name = crate::schema::drivers)]
pub struct DriverUpdateLocation {
    #[diesel(sql_type = Jsonb)]
    pub driver_location: serde_json::Value,
}

#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::ride_request)]
pub struct RideRequestUpdateLocation {
    #[diesel(sql_type = Jsonb)]
    pub pick_up: serde_json::Value
   
}

#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::ride_request)]
pub struct RideRequestUpdateDropOffLocation {
    #[diesel(sql_type = Jsonb)]
    pub drop_off: serde_json::Value
}


pub fn routes() -> Scope {
    web::scope("/matching")
        .route("/process-geolocation", web::post().to(process_geolocation))
}