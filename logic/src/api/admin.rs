use actix_web::{ web, Scope, HttpResponse, http::StatusCode };
use serde::{ Deserialize, Serialize };
use diesel::prelude::*;
use serde_json::{ json, Value };
use crate::db::{ DbPool };
use uuid::Uuid;
use crate::api::drivers::Driver;
use crate::api::errors::{ ApiError, blocking, json_error, message_body };
use crate::services::auth::{ AuthGuard, AuthUser };

pub async fn admin_dashboard(_auth: AuthGuard, pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::back_custom_users::dsl::back_custom_users as riders;
    use crate::schema::back_drivers::dsl::back_drivers as drivers;

    let counts = blocking(&pool, |conn| {
        let rider_count: i64 = riders.count().get_result(conn)?;
        let driver_count: i64 = drivers.count().get_result(conn)?;
        Ok((rider_count, driver_count))
    }).await;

    match counts {
        Ok((rider_count, driver_count)) => HttpResponse::Ok().json(json!({
            "status": "Ok",
            "rider_count": rider_count,
            "driver_count": driver_count
        })),
        Err(e) => e.response(),
    }
}


// Called by the app when signup completes. The rider id is the caller's auth.users id.
// Safe to retry: a second call for the same user is a no-op that still returns 200.
pub async fn create_rider(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<RiderRequest>,
) -> HttpResponse {
    use crate::schema::back_custom_users::dsl::{back_custom_users as riders, *};

    let new_rider = NewRider::new(auth.user_id, body.into_inner());

    let result = blocking(&pool, move |conn| {
        diesel::insert_into(riders)
            .values(new_rider)
            .on_conflict(rider_id)
            .do_nothing()
            .execute(conn)
            .map_err(signup_db_error)
    }).await;

    match result {
        Ok(1) => HttpResponse::Ok().json(message_body("Rider created")),
        Ok(_) => HttpResponse::Ok().json(message_body("Rider already exists")),
        Err(e) => e.response(),
    }
}

fn signup_db_error(e: diesel::result::Error) -> ApiError {
    use diesel::result::{ Error, DatabaseErrorKind };
    match e {
        // driver_id/rider_id conflicts are absorbed by on_conflict, so this is the email key
        Error::DatabaseError(DatabaseErrorKind::UniqueViolation, _) =>
            ApiError::Conflict("Email is already registered to another account".into()),
        other => other.into(),
    }
}

// No wallets yet: accounts are created without a pubkey. The column is NOT NULL, so this stores
// the same `{}` placeholder older accounts have; open_trip treats it as no pubkey.
fn no_pubkey() -> Value {
    json!({})
}

pub async fn get_riders(pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::back_custom_users::dsl::back_custom_users as riders;

    let results = blocking(&pool, |conn| {
        Ok(riders.limit(20).select(Rider::as_select()).load::<Rider>(conn)?)
    }).await;

    match results {
        Ok(data) => HttpResponse::Ok().json(data),
        Err(e) => e.response(),
    }
}


// Called by the app when signup completes. The driver id is the caller's auth.users id.
// Safe to retry: a second call for the same user is a no-op that still returns 200,
// so a lost response can't leave the rider stuck on "Setup Not Finished".
pub async fn create_driver(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<DriverRequest>,
) -> HttpResponse {
    use crate::schema::back_drivers::dsl::{back_drivers as drivers, *};

    let req = body.into_inner();
    if let Err(msg) = req.validate() {
        return json_error(StatusCode::BAD_REQUEST, msg);
    }

    let new_driver = NewDriver::new(auth.user_id, req);

    let result = blocking(&pool, move |conn| {
        diesel::insert_into(drivers)
            .values(new_driver)
            .on_conflict(driver_id)
            .do_nothing()
            .execute(conn)
            .map_err(signup_db_error)
    }).await;

    match result {
        Ok(1) => HttpResponse::Ok().json(message_body("Driver created")),
        Ok(_) => HttpResponse::Ok().json(message_body("Driver already exists")),
        Err(e) => e.response(),
    }
}


pub async fn get_drivers(pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::back_drivers::dsl::back_drivers as drivers;

    let results = blocking(&pool, |conn| {
        Ok(drivers.limit(20).select(Driver::as_select()).load::<Driver>(conn)?)
    }).await;

    match results {
        Ok(data) => HttpResponse::Ok().json(data),
        Err(e) => e.response(),
    }
}

//what admin::routes() returns
pub fn routes() -> Scope {
    web::scope("/admin")
        .route("/dashboard", web::get().to(admin_dashboard))
        .route("/get-riders", web::get().to(get_riders))
        .route("/create-riders", web::post().to(create_rider))
        .route("/get-drivers", web::get().to(get_drivers))
        .route("/create-drivers", web::post().to(create_driver))
}


#[derive(Deserialize)]
pub struct RiderRequest {
    pub name: String,
    pub email: String,
    pub phone: String,
}

#[derive(Queryable, Serialize, Deserialize, Selectable, Clone)]
#[diesel(table_name = crate::schema::back_custom_users)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Rider {
    pub rider_id: Uuid,
#[diesel(sql_type = diesel::sql_types::Jsonb)]
    pub rider_pubkey: serde_json::Value,
    pub name: String,
    pub email: String,
    pub phone: String,
}

#[derive(Insertable, Deserialize)]
#[diesel(table_name = crate::schema::back_custom_users)]
pub struct NewRider {
    pub rider_id: Uuid,
    pub rider_pubkey: serde_json::Value,
    pub name: String,
    pub email: String,
    pub phone: String,
}

impl NewRider {
    pub fn new(user_id: Uuid, req: RiderRequest) -> Self {
        Self {
            rider_id: user_id,
            rider_pubkey: no_pubkey(),
            name: req.name,
            email: req.email,
            phone: req.phone,
        }
    }
}


#[derive(Deserialize)]
pub struct DriverRequest {
    pub name: String,
    pub email: String,
    pub phone: String,
    pub license_number: Option<String>,
    pub vehicle_type: String,
    pub vehicle: Option<String>,
    pub account_number: Option<String>,
    pub bank_code: Option<String>,
}

impl DriverRequest {
    // The app sends exactly "Bike" or "EV"; EVs must also carry vehicle and licence details.
    fn validate(&self) -> Result<(), String> {
        let present = |v: &Option<String>| v.as_deref().map_or(false, |s| !s.trim().is_empty());
        match self.vehicle_type.as_str() {
            "Bike" => Ok(()),
            "EV" if present(&self.vehicle) && present(&self.license_number) => Ok(()),
            "EV" => Err("EV drivers must provide vehicle and license_number".into()),
            other => Err(format!("Unknown vehicle_type '{}', expected \"Bike\" or \"EV\"", other)),
        }
    }
}

#[derive(Insertable, Deserialize, Clone)]
#[diesel(table_name = crate::schema::back_drivers)]
pub struct NewDriver {
    pub driver_id: Uuid,
    pub driver_pubkey: serde_json::Value,
    pub name: String,
    pub email: String,
    pub phone: String,
    pub status: String,
    pub driver_location: serde_json::Value,
    pub license_number: Option<String>,
    pub vehicle_type: String,
    pub driver_response: serde_json::Value,
    pub vehicle: Option<String>,
    pub account_number: Option<String>,
    pub bank_code: Option<String>,
}

impl NewDriver {
    pub fn new(user_id: Uuid, req: DriverRequest) -> Self {
        Self {
            driver_id: user_id,
            driver_pubkey: no_pubkey(),
            name: req.name,
            email: req.email,
            phone: req.phone,
            status: "available".to_string(),
            // Same shapes as GeoPoint and DriverResponse::Timeout serialize to.
            driver_location: json!({ "lat": 0.0, "lng": 0.0, "name": "Earth" }),
            license_number: req.license_number,
            vehicle_type: req.vehicle_type,
            driver_response: json!("Timeout"),
            vehicle: req.vehicle,
            account_number: req.account_number,
            bank_code: req.bank_code,
        }
    }
}
