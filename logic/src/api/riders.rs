use actix_web::{ get, post, web, Scope, HttpResponse, Responder};
use serde::{ Deserialize, Serialize };
use diesel::dsl::sql;
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};
use lazy_static::lazy_static;
use diesel::prelude::*;
use diesel::pg::PgConnection;
use uuid::Uuid;
use tokio::time::{ Duration, sleep, timeout };
use reqwest::Client;
use std::collections::HashMap;
use crate::db::{ DbPool };
use crate::api::admin::{ Rider, NewRider };
use crate::services::{escrow, pricing::{ GeoPoint, minimum_distance_between_driver_and_pickup, distance_between }};
use crate::services::pricing;
use crate::services::notifications::calculate_eta;
use crate::api::drivers::{ DriverResponse, DriverResponsePayloadOut, DriverInfo, Driver };

lazy_static! {
    pub static ref DRIVER_RESPONSES: Mutex<HashMap<Uuid, oneshot::Sender<DriverResponse>>> = Mutex::new(HashMap::new());
}


pub async fn assign_driver_handler(
    body: web::Json<NewRideRequest>,
    pool: web::Data<DbPool>,
) -> HttpResponse {

    use crate::schema::drivers::dsl::*;

    let pick_up_geo2: GeoPoint = serde_json::from_value(body.pick_up.clone()).expect("Failed to convert pick_up JSON to GeoPoint");
    let drop_off_geo2: GeoPoint = serde_json::from_value(body.drop_off.clone()).expect("Failed to convert drop_off JSON to GeoPoint");

    let distance_km = pricing::distance_between(&pick_up_geo2, &drop_off_geo2);
    let estimated_time_min = body.estimated_time_min;
    let estimated_arrival: String = calculate_eta(estimated_time_min);

    //calculate price and estimated time based on ride type
    let ride_type2: RideType = serde_json::from_value(body.ride_type.clone())
                                         .expect("Failed to parse ride type");

    let estimated_price: i64 = match &ride_type2 {
        RideType::ASAP => pricing::calculate_asap(distance_km, estimated_time_min),
        RideType::ASAPEXPRESS => pricing::calculate_express(distance_km, estimated_time_min),
    };

    
     let cancel_reasons = vec![
        "Change of plans".to_string(),
        "Driver taking too long".to_string(),
        "Found alternate transport".to_string(),
        "Incorrect destination".to_string(),
    ];

    // Filter by ride type (maps to vehicle_type in DB)
    let vehicle_filter = match &ride_type2 {
        RideType::ASAP => vec!["EV".to_string()],
        RideType::ASAPEXPRESS => vec!["Bike".to_string()],
    };

    for _ in 1..=4 {
        let available_drivers = web::block({
            
            let pool = pool.clone();
            let vehicle_filter = vehicle_filter.clone();

            move || {

            let mut connection = match pool.get() {
                                 Ok(conn) => conn,
                                 Err(_) => return Err("Failed to get DB connection"),
    };

            drivers.filter(vehicle_type.eq_any(&vehicle_filter))
                   .filter(status.eq("available")) //treat
                   .limit(10)
                   .select(Driver::as_select())
                   .load::<Driver>(&mut connection)
                   .map_err(|_| "DB query error")
                }
                
            }).await;

        match available_drivers {
            Ok(Ok(list)) if !list.is_empty() => {
                for driver in list {

                        let driver_info: DriverInfo = DriverInfo {
                            name: driver.name.clone(),
                            phone: driver.phone.clone(),
                            //rating: driver.rating,
                            vehicle: driver.vehicle.clone(),
                            license_number: driver.license_number.clone(),
                        };


                        let pick_up_geo: GeoPoint = serde_json::from_value(body.pick_up.clone())
                                                                .expect("Failed to convert pick_up JSON to GeoPoint");

                    if minimum_distance_between_driver_and_pickup(
                        pick_up_geo,
                        driver.location().clone(),
                    ) {
                        // ✅ Optional: mark driver as assigned
                        // let _ = diesel::update(drivers.filter(id.eq(driver.id)))
                        //     .set(status.eq("assigned"))
                        //     .execute(&mut connection);

                        // ✅ Send notification to driver app/server
                    let (tx, rx) = oneshot::channel();
                    DRIVER_RESPONSES.lock().await.insert(driver.driver_id, tx);

                    // Notify driver
                    let notify_url = format!("http://localhost:8080/notify-driver/{}", driver.driver_id);
                    let client = reqwest::Client::new();
                    let resp = client.post(&notify_url).json(&body).send().await.expect("Failed to send notification");

                if resp.status().is_success() {
                // Wait for that driver's response
                    
    match timeout(Duration::from_secs(50), rx).await {
    Ok(Ok(other_driver_response)) => match other_driver_response {
        DriverResponse::Accepted => {
            let ride_assignment = RideAssignment {
                estimated_price,
                estimated_time_min,
                estimated_arrival,
                validation_status: "driver is on his way.".into(),
                driver_assigned: Some(driver_info),
                message: Some("your package will be with you shortly.".into()),
                cancel_ride: Some(cancel_reasons),
            };
            println!("Driver {} accepted ride", driver.driver_id);
            return HttpResponse::Ok().json(ride_assignment);
        }
        DriverResponse::Rejected => {
            println!("Driver {} rejected ride", driver.driver_id);
            continue;
        }
        DriverResponse::Timeout => {
            println!("Driver {} did not respond", driver.driver_id);
            continue;
        }
    },
    Ok(Err(_recv_error)) => {
        println!("Driver {} channel failed", driver.driver_id);
        continue;
    },
    Err(_elapsed) => {
        println!("Driver {} timed out", driver.driver_id);
        continue;
    },
}

                        
                        
             }

        }
    
    }
    
}
         Ok(Ok(_)) => {
                // no driver found, wait and retry
                sleep(Duration::from_millis(500)).await;
            },
            Ok(Err(inner_err)) => eprintln!("Db error: {}", inner_err),
            
            Err(e) => {
                eprintln!("DB error: {}", e);
                return HttpResponse::InternalServerError().body("Database query failed");
            },
    }
}

    HttpResponse::NotFound().body("No suitable driver available after 4 attempts")
}





pub async fn driver_response(payload: web::Json<DriverResponsePayloadOut>, driver_id: web::Path<Uuid>) -> impl Responder {
    let data = payload.into_inner();
    let driver_id = driver_id.into_inner();

    // Extract driver_id and response together
    let (driver_id, response) = match data {
        DriverResponsePayloadOut::Accepted {
            rider_id: _rider_id, // keep if needed
            driver_id,
            message: _message,   // keep if needed
        } => (driver_id, DriverResponse::Accepted),

        DriverResponsePayloadOut::Rejected {
            driver_id,
        } => (driver_id, DriverResponse::Rejected),
    };

    // Use driver_id to look up the response channel
    if let Some(tx) = DRIVER_RESPONSES.lock().await.remove(&driver_id) {
        let _ = tx.send(response);
    }

    HttpResponse::Ok().json("Response received")
}





 pub fn validate_rider_account(
    connection: &mut PgConnection,
    rider_id: Uuid
) -> Result<Rider, String> {
    use crate::schema::riders::dsl::*;

    let rider: Rider = riders
        .find(rider_id)
        .select(Rider::as_select())
        .first::<Rider>(connection)
        .map_err(|_| "Rider not found".to_string())?;

    Ok(rider)
}

pub async fn request_ride(
    pool: web::Data<DbPool>,
    body: web::Json<CreateRideRequest>,
) -> HttpResponse {
    use crate::schema::ride_request::dsl::*;

    let req = body.into_inner();

    let rider_uuid = req.rider_id;


    let new_ride_request = NewRideRequest::new(req);


    let validation_status = web::block({
    let pool = pool.clone();
    move || {
        let mut conn = pool.get().unwrap();
        validate_rider_account(&mut conn, rider_uuid)
            .map_err(|e| format!("rider validation error: {}", e))?;

        diesel::insert_into(ride_request)
            .values(new_ride_request)
            .execute(&mut conn)
            .map_err(|e| format!("DB insert error: {}", e))

    }
}).await;

let status = match validation_status {
    Ok(Ok(status)) => status,

    Ok(Err(db_err)) => {
        return HttpResponse::InternalServerError()
            .body(format!("DB error: {}", db_err));
    }

    Err(block_err) => {
        return HttpResponse::InternalServerError()
            .body(format!("Blocking error: {}", block_err));
    }
};

HttpResponse::Ok().json(status)


}

pub fn routes() -> Scope {
    web::scope("/riders")
        .route("/assign-driver", web::get().to(assign_driver_handler))
        .route("/wait-driver-response", web::get().to(driver_response))
        .route("/ride-request", web::post().to(request_ride))
}

#[derive(Deserialize, Serialize, Clone)]
pub enum RideType {
    ASAP,
    ASAPEXPRESS,
}


///The whole thing needs work
#[derive(Deserialize)]
pub struct CreateRideRequest {
    pub rider_id: Uuid,
    pub pick_up: GeoPoint,
    pub drop_off: GeoPoint,
    pub ride_type: RideType,
    pub payment_method: String,
    pub items: Vec<ItemDetails>,
}

#[derive(Deserialize, Serialize, Insertable, Clone)]
#[diesel(table_name = crate::schema::ride_request)]
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
}


impl NewRideRequest {
    pub fn new(req: CreateRideRequest) -> Self {

        let distance_km = distance_between(&req.pick_up, &req.drop_off);
        let estimated_time_min =
            pricing::estimated_time_min(distance_km, &req.ride_type);

        let estimated_price = match req.ride_type {
            RideType::ASAP => pricing::calculate_asap(distance_km, estimated_time_min),
            RideType::ASAPEXPRESS => pricing::calculate_express(distance_km, estimated_time_min),
        };

        Self {
            request_id: Uuid::new_v4(),
            rider_id: req.rider_id,
            pick_up: serde_json::to_value(&req.pick_up).expect("serialize pick_up"),
            drop_off: serde_json::to_value(&req.drop_off).expect("serialize drop_off"),
            ride_type: serde_json::to_value(&req.ride_type).expect("serialize ride_type"),
            items: serde_json::to_value(&req.items).expect("serialize items"),
            estimated_price,
            distance_km,
            estimated_time_min,
            payment_method: req.payment_method,
        }
    }
}


#[derive(Serialize, Deserialize, Clone, Queryable, Selectable)]
#[diesel(table_name = crate::schema::ride_request)]
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
}



#[derive(Serialize, Deserialize, Clone)]
pub struct RideAssignment {
    pub estimated_arrival: String,
    pub estimated_time_min: i32,
    pub estimated_price: i64,
    pub validation_status: String,
    pub driver_assigned: Option<DriverInfo>,
    pub message: Option<String>, 
    pub cancel_ride: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ItemDetails {
    pub name: String,
    pub price: u64,
    pub dimensions: (f64, f64, f64), // (length, width, height) in cm
    pub quantity: u32,
    pub weight: f64, // in kg
    
}

impl ItemDetails {
    pub fn max_dimensions(&self) -> Result<(), String> {
        let (length, width, height) = self.dimensions;
        

        if (length, width, height) > (13.0, 13.0, 13.0)  {
            return Err("Sorry, we cant handle items of this size.".into());
        }
            Ok(())
    } 

    pub fn aggregate_quantity(&self) -> Result<(), String> {
            let mut total_number_of_items = self.quantity;
            let max_number_of_items = 10;

            if total_number_of_items > max_number_of_items {
                return Err("Too many Items".into());
            }

            Ok(())
        }

    pub fn max_weight(&self) -> Result<(), String> {
         let total_weight = self.weight * self.quantity as f64;
        
         if total_weight > 5.0 {
            return Err("Sorry, we cant handle items of this size.".into());
        }

        Ok(())
   }


}

