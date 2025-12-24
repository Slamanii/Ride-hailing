use actix_web::{ web, HttpResponse, Scope };
use serde::{ Deserialize, Serialize };
use serde_json::Value;
use uuid::Uuid;
use diesel::prelude::*;
use chrono::Utc;
use sha2::{Sha256, Digest};
use crate::schema::trips::dsl::*;
use crate::api::drivers::Driver;
use crate::api::riders::{ RideRequest };
use crate::api::admin::Rider;
use crate::db::DbPool;
use diesel::pg::PgConnection;




pub async fn create_trip(
    pool: web::Data<DbPool>,
    body: web::Json<Trip>
) -> HttpResponse {
    
    let trip = body.into_inner();

    let result = web::block({

        let pool = pool.clone();
        move || {

            let mut conn = pool.get().expect("Failed to get connection");

            let mut hasher = Sha256::new();
            hasher.update(Uuid::new_v4().as_bytes());

            let trip_id_bytes: [u8; 32] = hasher.finalize().as_slice().try_into().unwrap();
        

        diesel::insert_into(trips)
            .values((
                trip_id.eq(trip_id_bytes), // [u8; 32] as bytes
                rider_id.eq(trip.rider_id), // UUID
                reference.eq(trip.reference),
                status.eq("ongoing"),
                pick_up.eq(trip.pick_up), 
                drop_off.eq(trip.drop_off), 
                driver_location.eq(trip.driver_location), 
                rider_pubkey.eq(trip.rider_pubkey), // Pubkey as string
                driver_pubkey.eq(trip.driver_pubkey), // Pubkey as string
                driver_id.eq(trip.driver_id),
                start_ts.eq(trip.start_ts),
                end_ts.eq(trip.end_ts),
                item.eq(trip.item), // JSONB
                distance_km.eq(trip.distance_km),
                fare_estimate.eq(trip.fare_estimate),
                fare_lamports.eq(trip.fare_lamports),
                rider_email.eq(trip.rider_email),

            ))
            .execute(&mut conn).map(|e| e.to_string());
        }
    })
    .await;

    match result {
        Ok(_) => HttpResponse::Ok().body("Trip created"),
        Err(e) => HttpResponse::InternalServerError().body(format!("Error: {}", e)),
    }
}



pub async fn update_trip(
    pool: web::Data<DbPool>,
    rider_uuid: web::Path<Uuid>,
    body: web::Json<Trip>,
) -> HttpResponse {
    use crate::schema::trips::dsl::*;

    let rider_id_val = rider_uuid.into_inner();
    let trip = body.into_inner();

    let result = web::block({
        let pool = pool.clone();

        move || -> Result<usize, String> {
            let mut conn = pool.get().map_err(|e| e.to_string())?;

            diesel::update(trips.filter(rider_id.eq(rider_id_val)))
                .set((
                    status.eq(trip.status),
                    end_ts.eq(trip.end_ts),
                    fare_estimate.eq(trip.fare_estimate),
                    fare_lamports.eq(trip.fare_lamports),
                ))
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


pub async fn get_trip(
    pool: web::Data<DbPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let reference_value = path.into_inner();

    let result = web::block({
        let pool = pool.clone();
        move || {
            let mut conn = pool.get().expect("Failed to get connection");
            get_trip_by_reference(&mut conn, &reference_value)
        }
    })
    .await;

    match result {
        Ok(Ok(trip)) => HttpResponse::Ok().json(trip),

        Ok(Err(diesel::result::Error::NotFound)) => {
            HttpResponse::NotFound().body("Trip not found")
        }

        Ok(Err(e)) => {
            eprintln!("DB error: {:?}", e);
            HttpResponse::InternalServerError().body("Database error")
        }

        Err(e) => {
            eprintln!("Blocking error: {:?}", e);
            HttpResponse::InternalServerError().body("Server error")
        }
    }
}

// in riders.rs
pub fn get_trip_by_reference(conn: &mut PgConnection, ref_str: &str) -> QueryResult<Trip> {
    use crate::schema::trips::dsl::*;


    trips.filter(reference.eq(ref_str))
         .select(Trip::as_select())
         .first::<Trip>(conn)
}





// from escrow .rs or to escrow.rs
#[derive(Deserialize)]
struct CreateTripInput {
    pickup: String,
    destination: String,
    fare_estimate: i64,
    rider_id: Uuid,
}


#[derive(Serialize, Deserialize, Queryable, Selectable, Debug, Clone)]
#[diesel(table_name = crate::schema::trips)]
pub struct Trip {
    pub trip_id: Vec<u8>,
    pub rider_id: Uuid,
    pub reference: String,
    pub pick_up: String,
    pub drop_off: String,
    pub driver_location: String,
    pub rider_pubkey: String,
    pub driver_pubkey: String,
    pub driver_id: Uuid,
    pub status: String,
    pub start_ts: i64,
    pub end_ts: Option<i64>,
    pub distance_km: f64,
    pub item: serde_json::Value,  //treat in sql
    pub fare_estimate: Option<i64>,
    pub fare_lamports: Option<i64>,
    pub rider_email: String,
}
//you havent implemented trip( pull from riders, drivers & admin) ------- maybe this should be from db as well who knows remember to check it

impl Trip {

    pub fn new(drv: Driver, rdr: Rider, req2: RideRequest) -> Self {
        
        let start_ts_value = Utc::now().timestamp() as i64;

       

        Self {
            trip_id: Vec::new(),  //this should be a UUID in bytes
            rider_id: rdr.rider_id,
            reference: Uuid::new_v4().to_string(),
            pick_up: req2.pick_up.to_string(),
            drop_off: req2.drop_off.to_string(),
            driver_location: drv.driver_location.to_string(),
            rider_pubkey: rdr.rider_pubkey.to_string(),
            driver_pubkey: drv.driver_pubkey.to_string(),
            driver_id: drv.driver_id,
            status: "Ongoing".to_string(),
            start_ts: start_ts_value,
            end_ts: None,
            distance_km: req2.distance_km,
            item: req2.items.clone(),
            fare_estimate: Some(req2.estimated_price),
            fare_lamports: None,  //we need to calculate this in pricing using fare_estimate
            rider_email: rdr.email,


        }
    }

    pub fn update_status(&mut self)  {
        
        if self.driver_location == self.drop_off {
            self.status = "Completed".to_string();
            self.end_ts = Some(Utc::now().timestamp() as i64);
        }
    }
   
    pub fn compute_fare_lamports(&mut self) {
        if let Some(estimate) = self.fare_estimate {
            
            let lamports_per_ngn: f64 = 128.0;

            self.fare_lamports = Some((estimate as f64 * lamports_per_ngn) as i64);
        }
    }
}




pub fn routes() -> Scope {
    web::scope("/trips")
        .route("/create-trip", web::post().to(create_trip))
        .route("/update-trip", web::post().to(update_trip))
        .route("/get-trip/{reference}", web::get().to(get_trip))
}

