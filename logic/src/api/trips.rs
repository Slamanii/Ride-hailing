use actix_web::{ web, HttpResponse, Scope };
use crate::services::auth::AuthUser;
use serde::{ Deserialize, Serialize };
use uuid::Uuid;
use diesel::prelude::*;
use chrono::Utc;
use sha2::{Sha256, Digest};
use crate::api::drivers::Driver;
use crate::api::riders::{ RideRequest };
use crate::api::admin::Rider;
use crate::api::errors::{ ApiError, blocking };
use crate::db::DbPool;
use diesel::pg::PgConnection;

// Trips are written only by the order flow (orders::open_trip, await_payout, close_trip):
// their status drives the driver payout, so no client route creates or updates them.

// The rider or the driver of the trip; anyone else gets the same 404 as a missing trip.
pub async fn get_trip(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let me = auth.user_id;
    let reference_value = path.into_inner();

    let result = blocking(&pool, move |conn| {
        get_trip_by_reference(conn, &reference_value)?
            .filter(|trip| trip.rider_id == me || trip.driver_id == me)
            .ok_or_else(|| ApiError::NotFound("Trip not found".into()))
    }).await;

    match result {
        Ok(trip) => HttpResponse::Ok().json(trip),
        Err(e) => e.response(),
    }
}

pub fn get_trip_by_reference(conn: &mut PgConnection, ref_str: &str) -> QueryResult<Option<Trip>> {
    use crate::schema::back_trips::dsl::*;

    back_trips.filter(reference.eq(ref_str))
        .select(Trip::as_select())
        .first::<Trip>(conn)
        .optional()
}

pub fn get_trip_by_order_ref(conn: &mut PgConnection, order_ref_val: Uuid) -> QueryResult<Option<Trip>> {
    use crate::schema::back_trips::dsl::*;

    back_trips.filter(order_ref.eq(order_ref_val))
        .select(Trip::as_select())
        .first::<Trip>(conn)
        .optional()
}


#[derive(Deserialize, Clone)]
pub struct CreateTripInput {
    pub trip_id: [u8; 32],
    pub rider_id: Uuid,
    pub reference: String,
    pub pick_up: serde_json::Value,
    pub drop_off: serde_json::Value,
    pub driver_location: serde_json::Value,
    pub rider_pubkey: Option<String>,
    pub driver_pubkey: Option<String>,
    pub driver_id: Uuid,
    pub status: String,
    pub start_ts: i64,
    pub end_ts: Option<i64>,
    pub distance_km: f64,
    pub item: serde_json::Value,
    pub fare_estimate: Option<i64>,
    pub fare_lamports: Option<i64>,
    pub order_ref: Option<Uuid>,
    pub rider_email: String,
}

impl CreateTripInput {

    pub fn new(drv: Driver, rdr: Rider, req2: RideRequest) -> Self {
        let trip_id_bytes: [u8; 32] = Sha256::digest(Uuid::new_v4().as_bytes()).into();

        Self {
            trip_id: trip_id_bytes,
            rider_id: rdr.rider_id,
            reference: Uuid::new_v4().to_string(),
            pick_up: req2.pick_up.clone(),
            drop_off: req2.drop_off.clone(),
            driver_location: drv.driver_location.clone(),
            rider_pubkey: rdr.rider_pubkey.as_str().map(str::to_string),
            driver_pubkey: drv.driver_pubkey.as_str().map(str::to_string),
            driver_id: drv.driver_id,
            status: "Ongoing".to_string(),
            start_ts: Utc::now().timestamp(),
            end_ts: None,
            distance_km: req2.distance_km,
            item: req2.items.clone(),
            fare_estimate: Some(req2.estimated_price),
            fare_lamports: None,
            rider_email: rdr.email,
            order_ref: req2.order_ref,
        }
    }
}


#[derive(Serialize, Deserialize, Queryable, Selectable, Debug, Clone)]
#[diesel(table_name = crate::schema::back_trips)]
pub struct Trip {
    pub trip_id: Vec<u8>,
    pub rider_id: Uuid,
    pub reference: String,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub pick_up: serde_json::Value,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub drop_off: serde_json::Value,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub driver_location: serde_json::Value,
    // NULL until riders and drivers have real wallets.
    pub rider_pubkey: Option<String>,
    pub driver_pubkey: Option<String>,
    pub driver_id: Uuid,
    pub status: String,
    pub start_ts: i64,
    pub end_ts: Option<i64>,
    pub distance_km: f64,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub item: serde_json::Value,
    pub fare_estimate: Option<i64>,
    pub fare_lamports: Option<i64>,
    pub order_ref: Option<Uuid>,
    pub rider_email: String,
}

impl Trip {

    pub fn update_status(&mut self) {
        if self.driver_location == self.drop_off {
            self.status = "Completed".to_string();
            self.end_ts = Some(Utc::now().timestamp());
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
        .route("/get-trip/{reference}", web::get().to(get_trip))
}
