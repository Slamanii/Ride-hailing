use actix_web::{rt::{time}, get, post, web, Scope, HttpResponse, Responder};
use serde::{Deserialize, Serialize};
use diesel::prelude::*;
use crate::db::{PgPool};
use crate::api::admin::{Rider, NewRider, Driver};
use crate::services::{pricing, escrow};
use crate::services::notifications::calculate_eta;


async fn assign_driver(ride_type: &RideType, pool: &PgPool) ->
Result<DriverInfo, String> {
    //Source for driver data from db
    use crate::schema::drivers::dsl::*;
    let connection = &mut pool.get.expect("DB connection failed");

    //Filter drivers by ride type/ vehicle type
    let vehicle_filter = match ride_type {
        RideType::ASAP => vec!["EV".to_string()],
        RideType::ASAPEXPRESS =>  vec!["Bike".to_string()],
    };
     
    for attempt in 1..=4 {
        let available_driver = drivers
        .filter(vehicle.eq_any(&vehicle_filter))
        .limit(1)
        .load::<Driver>(&mut connection)
        .map_err(|e| e.to_string())?
        .into_iter()
        .next();

        if let Some(driver) = available_driver {

    // Here you could mark driver as “assigned” in DB

            return Ok(DriverInfo {
                id: driver.id,
                name: driver.name,
                phone: driver.phone,
                vehicle: driver.vehicle,
            });
        }

        time::sleep(std::time::Duration::from_millis(500)).await;
    }
    
    Err("No driver available after 4 attempts, please try again".into())
}

#[post("/riders/request_ride")]

fn validate_rider_account(
    conn: &mut PgConnection,
    rider_id: uuid::Uuid
) -> Result<Rider, String> {
    use crate::schema::riders::dsl::*;

    let rider: Rider = riders
        .find(rider_id)
        .first::<Rider>(conn)
        .map_err(|_| "Rider not found".to_string())?;

    if rider.status != "active" {
        return Err("Rider account inactive".into());
    }

    Ok(rider)
}


async fn request_ride(
    pool: web::Data<PgPool>,
    body: web::Json<RideRequest>,
) -> HttpResponse {
    let req = body.into_inner();



    //validate rider account (simplified)
    if validate_rider_account(req.rider_id, &mut pool.get().unwrap()) != "active" {
        return HttpResponse::Forbidden().json(RideAssignment {
            estimated_price: 0.0,
            estimated_time_min: 0,
            estimated_arrival: None,
            validation_status: "failed".into(),
            driver_assigned: None,
            message: Some("Rider account inactive".into())
        });
    }

    //calculate price and estimated time based on ride type
    let (estimated_price, estimated_time_min) = match req.ride_type {
        RideType::ASAP => pricing::calculate_asap(&req.pick_up, &req.drop_off),
        RideType::ASAPEXPRESS => pricing::caluclate_express(&req.pick_up, &req.drop_off),
    }

    
    //Vaidate items
    if let Some(items) = &req.items {
        for item in items {
            if item.quantity <= 0 || item.weight <= 0.0 {
                return HttpResponse::BadRequest().json(RideAssignment {
                    estimated_price: 0.0,
                    estimated_time_min: 0,
                    estimated_arrival: None,
                    validation_status: "failed".into(),
                    driver_assigned: None,
                    message: Some(format!("Invalid item: {:?}", item)),
                });
            }
        }
    }

    //verify payment /escrow
    if !escrow::verify_payment(&req.rider_id, &req.payment_method, estimated_price) {
        return HttpResponse::PaymentRequired().json(RideAssignment {
            estimated_price,
            estimated_time_min,
            estimated_arrival: None,
            validation_status: "failed".into(),
            driver_assigned: None,
            message: Some("Payment verification failed".into()),
        });
    }

     let eta_timestamp = calculate_eta(estimated_time_min);
    
    //Assign driver
    match assign_driver(&req.ride_type, &pool).await {
        Ok(driver) => HttpResponse::Ok().json(RideAssignment {
            estimated_price,
            estimated_time_min,
            estimated_arrival: Some(eta_timestamp),
            validation_status: "failed".into(),
            driver_assigned: None,
            message: Some("Payment verification failed".into()),
        })
    },
    Err(err_msg) => HttpResponse::ServiceUnavailable().json(RideAssignment {
        estimated_price,
        estimated_time_min,
        estimated_arrival: Some(eta_timestamp),
        validation_status: "failed".into(),
        driver_assigned: None,
        message: Some(err_msg),
    }),

}

#[derive(Deserialize)]
pub enum RideType {
    ASAP,
    ASAPEXPRESS,
}

#[derive(Deserialize)]
pub struct RideRequest {
    pub rider_id: i32,
    pub pick_up: String,
    pub drop_off: String,
    pub ride_type: RideType,
    pub items: Vec<ItemDetails>,
    pub payment_method: String,
} 

#[derive(Serialize)]
pub struct RideAssignment {
    pub estimated_arrival: Option<String>,
    pub estimated_time_min: u32,
    pub estimated_price: u64,
    pub validation_status: String,
    pub driver_assigned: Option<DriverInfo>,
    pub message: Option<String>,
}

#[derive(Deserialize)]
pub struct ItemDetails {
    pub name: String,
    pub quantity: u32,
    pub price: u64,
    pub weight: f64, // in kg
    pub dimensions: (f64, f64, f64), // (length, width, height) in cm
}

#[derive(Serialize)]
pub struct DriverInfo {
    pub id: i32,
    pub name: String,
    pub phone: String,
    pub vehicle: Option<String>,
}





// from escrow .rs or to escrow.rs
use serde::Serialize;
use solana_sdk::pubkey::Pubkey;

#[derive(Serialize, Clone)]
pub struct Trip {
    pub ride_id: [u8; 32],
    pub rider_pubkey: Pubkey,
    pub driver_id: String,
    pub start_ts: u64,
    pub end_ts: u64,
    pub start_lat: i32,
    pub start_lon: i32,
    pub end_lat: i32,
    pub end_lon: i32,
    pub distance_m: u32,
    pub fare_lamports: u64,
}

pub async fn get_trip_by_reference(reference: &str) -> anyhow::Result<Trip> {
    // fetch from database
    Ok(Trip {
        ride_id: sha2::Sha256::digest(reference.as_bytes()).into(),
        rider_pubkey: Pubkey::new_unique(),
        driver_id: "driver123".into(),
        start_ts: 1696000000,
        end_ts: 1696003000,
        start_lat: 64000000,
        start_lon: 3400000,
        end_lat: 64010000,
        end_lon: 3401000,
        distance_m: 2500,
        fare_lamports: 1_000_000,
    })
}
