use actix_web::{  web, Scope, HttpResponse };
use serde::{ Serialize, Deserialize };
use diesel::prelude::*;
use diesel::sql_types::Jsonb;
use uuid::Uuid;
use serde_json::to_string;
use serde_json::Value;
use crate::db::{ DbPool };
use diesel::pg::PgConnection;
use crate::api::riders::{ NewRideRequest, RideType };
use crate::services::{ pricing::GeoPoint, escrow };


pub async fn update_driver(
    pool: web::Data<DbPool>,
    body: web::Json<Driver>,
) -> HttpResponse {
    use crate::schema::drivers::dsl::*;

    let driver = body.into_inner();


    let result = web::block({

            let pool = pool.clone();
            let driver = driver.clone();
        
            move || {
        let mut conn = pool.get().expect("Error connecting to db");

        let filter_value = driver.driver_location.clone();
        let filter_value2 = driver.driver_response.clone();


        diesel::update(drivers.filter(driver_id.eq(driver.driver_id)))
                              .set(DriverUpdate {
                                    driver_location: filter_value,
                                    driver_response: filter_value2,
                                })
                              .execute(&mut conn)
                              .map_err(|e| e.to_string())
        }
    })
    .await;

        match result {
        Ok(Ok(rows)) if rows > 0 => HttpResponse::Ok().json(rows),

        Ok(Ok(_)) => HttpResponse::Ok().body("No rows updated"),

        Ok(Err(db_err)) => {
            HttpResponse::InternalServerError().body(format!("DB error: {}", db_err))
        }

        Err(block_err) => {
            HttpResponse::InternalServerError()
                .body(format!("Blocking error: {}", block_err))
        }
    }

}


pub fn verify_driver_account(connection: &mut PgConnection, driver_id: Uuid) -> Result<Driver, String> {
    use crate::schema::drivers::dsl::*;

    let driver: Driver = drivers
        .find(&driver_id)
        .select(Driver::as_select())
        .first::<Driver>(connection)
        .map_err(|e| format!("Driver not found: {}", e))?;

        if driver.status != "available" {
            return Err("Driver account is not available".into());
        }

        Ok(driver)
}  

pub async fn notify_driver_handler(pool: web::Data<DbPool>, body: web::Json<NewRideRequest>, driver_uuid: web::Path<Uuid>) -> HttpResponse {

    let driver_uuid = driver_uuid.into_inner();


    let driver_checked = web::block({
        
            let pool = pool.clone();

        move || {
        
        let mut connection = pool.get().expect("Error connecting to db");

        verify_driver_account(&mut connection, driver_uuid) 
        .map_err(|e| e.to_string())
    }

}).await;
    
let driver = match driver_checked {
    Ok(Ok(driver)) => driver,

    Ok(Err(msg)) => {
        return HttpResponse::BadRequest().body(msg);
    }

    Err(block_err) => {
        return HttpResponse::InternalServerError()
            .body(format!("Blocking error: {}", block_err));
    }
};

//use tcp or websocket to notify driver here
    HttpResponse::Ok().json(body.into_inner())

}

pub fn get_ongoing_trips_count(connection: &mut PgConnection, driver_uuid: Uuid) -> Result<i64, String> {

    use crate::schema::trips::dsl::*;

    let count: i64 = trips
        .filter(driver_id.eq(&driver_uuid))
        .filter(status.eq("ongoing"))
        .count()
        .get_result(connection)
        .map_err(|e| format!("DB error: {}", e))?;

    Ok(count)
}

pub async fn driver_response_handler(
    pool: web::Data<DbPool>,
    payload: web::Json<DriverResponsePayload>,
) -> HttpResponse {
    let payloads = payload.into_inner();
    let driver_response = payloads.response.clone();
    let rider_id = payloads.rider_id;

    let result = web::block({
        let driver_id = payloads.driver_id;
        let pool = pool.clone();

        move || {
            let mut connection = pool.get().expect("Error connecting to db");
            let driver = verify_driver_account(&mut connection, driver_id)
                .map_err(|e| e.to_string());

            let trip_count = get_ongoing_trips_count(&mut connection, driver_id.clone()).expect("Error getting trip count");

            Ok::<_, String>(DriverContext {
                driver: driver.expect("Driver not found"),
                ongoing_trip_count: trip_count,
            })
        }
    })
    .await;

    let ctx = match result {
        Ok(Ok(ctx)) => ctx,
        Ok(Err(msg)) => return HttpResponse::BadRequest().body(msg),
        Err(_) => return HttpResponse::InternalServerError().body("Blocking thread failed"),
    };

    // ✅ Apply trip limit logic ONCE
    if ctx.driver.vehicle_type == "Bike" && ctx.ongoing_trip_count >= 2 {
        return HttpResponse::BadRequest()
            .body("Bike drivers can only handle 2 ongoing trips");
    }

    if ctx.driver.vehicle_type != "Bike" && ctx.ongoing_trip_count >= 1 {
        return HttpResponse::BadRequest()
            .body("Standard drivers can only handle 1 ongoing trip");
    }

    // ✅ Now handle driver response
    match driver_response.as_str() {
        "accepted" => HttpResponse::Ok().json(
            DriverResponsePayloadOut::Accepted {
                rider_id,
                driver_id: ctx.driver.driver_id,
                message: "Ride confirmed".to_string(),
            },
        ),

        "rejected" => HttpResponse::Ok().json(
            DriverResponsePayloadOut::Rejected {
                driver_id: ctx.driver.driver_id,
            },
        ),

        _ => HttpResponse::BadRequest().body("Invalid response"),
    }
}


pub fn routes() -> Scope {
    web::scope("/drivers")
        .route("/notify-driver", web::get().to(notify_driver_handler))
        .route("/driver-response", web::get().to(driver_response_handler))
        .route("/update-driver", web::post().to(update_driver))
}

#[derive(Deserialize)]
struct DriverContext {
    driver: Driver,
    ongoing_trip_count: i64,
}

#[derive(Deserialize, Serialize)]
pub struct DriverResponsePayload {
    driver_id: Uuid,
    rider_id: Uuid,
    response: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum DriverResponsePayloadOut {
    Accepted {
        rider_id: Uuid,
        driver_id: Uuid,
        message: String,
    },
    Rejected {
        driver_id: Uuid,
    },
}

#[derive(Deserialize, Serialize, Clone)]
pub enum DriverResponse {
    Accepted,
    Rejected,
    Timeout,
}


#[derive(Deserialize, Serialize, Clone)]
pub struct DriverInfo {
    pub name: String,
    pub phone: String,
   // pub rating: Option<f32>,
    pub vehicle: Option<String>,
    pub license_number: Option<String>,
}
 

#[derive(Debug, Queryable, Serialize, Deserialize, Selectable, Clone)]
#[diesel(table_name = crate::schema::drivers)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Driver {
    pub driver_id: Uuid,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub driver_pubkey: serde_json::Value,   ///treat with Privy
    pub name: String,
    pub email: String,
    pub phone: String,
    pub status: String,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub driver_location: serde_json::Value,
    pub license_number: Option<String>,
    pub vehicle_type: String,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub driver_response: serde_json::Value,   ///treat with Privy
    pub vehicle: Option<String>, 
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

    pub fn location(&self) -> GeoPoint {
        serde_json::from_value(self.driver_location.clone())
            .expect("invalid GeoPoint in DB")
    }
}

#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::drivers)]
pub struct DriverUpdate {
    pub driver_location: serde_json::Value,
    pub driver_response: serde_json::Value,
}



