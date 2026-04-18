use actix_web::{ web, Scope, HttpResponse,  };
use serde::{ Deserialize, Serialize };
use diesel::prelude::*;
use serde_json::Value;
use crate::db::{ DbPool };
use crate::services::pricing::{ GeoPoint };
use uuid::Uuid;
use crate::api::drivers::{ Driver, DriverResponse };

pub async fn admin_dashboard(pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::riders::dsl::*;
    use crate::schema::drivers::dsl::*;

    // Count riders
    let rider_count = match web::block({
        let pool = pool.clone();
        move || -> Result<i64, String> {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            riders.count().get_result(&mut conn).map_err(|e| e.to_string())
        }
    }).await {
        Ok(Ok(count)) => count,
        Ok(Err(e)) => return HttpResponse::InternalServerError().body(format!("DB error: {}", e)),
        Err(_) => return HttpResponse::InternalServerError().body("Error counting riders"),
    };

    // Count drivers
    let driver_count = match web::block({
        let pool = pool.clone();
        move || -> Result<i64, String> {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            drivers.count().get_result(&mut conn).map_err(|e| e.to_string())
        }
    }).await {
        Ok(Ok(count)) => count,
        Ok(Err(e)) => return HttpResponse::InternalServerError().body(format!("DB error: {}", e)),
        Err(_) => return HttpResponse::InternalServerError().body("Error counting drivers"),
    };

    // Return combined JSON
    HttpResponse::Ok().json(serde_json::json!({
        "status": "Ok",
        "rider_count": rider_count,
        "driver_count": driver_count
    }))
}


pub async fn create_rider(
    pool: web::Data<DbPool>,
    body: web::Json<RiderRequest>,
) -> HttpResponse {
    use crate::schema::riders::dsl::*;

    let new_rider = NewRider::new(body.into_inner());

    let result = web::block({
        let pool = pool.clone();
        move || -> Result<usize, String> {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            diesel::insert_into(riders)
                .values(new_rider)
                .execute(&mut conn)
                .map_err(|e| e.to_string())
        }
    }).await;

    match result {
        Ok(Ok(_rows)) => HttpResponse::Ok().json("Rider created"),
        Ok(Err(db_err)) => HttpResponse::BadRequest().body(format!("DB error: {}", db_err)),
        Err(block_err) => HttpResponse::InternalServerError().body(format!("Threadpool error: {}", block_err)),
    }
}

pub async fn get_riders(pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::riders::dsl::*;

    let results = web::block({
        let pool = pool.clone();
        move || -> Result<Vec<Rider>, String> {
            let mut connection = pool.get().map_err(|e| e.to_string())?;
            riders.limit(20)
                  .select(Rider::as_select())
                  .load::<Rider>(&mut connection)
                  .map_err(|e| e.to_string())
        }
    }).await;

    match results {
        Ok(Ok(data)) => HttpResponse::Ok().json(data),
        Ok(Err(db_err)) => {
            eprintln!("DB error: {:?}", db_err);
            HttpResponse::InternalServerError().body("Database error")
        }
        Err(blocking_err) => {
            eprintln!("Blocking error: {:?}", blocking_err);
            HttpResponse::InternalServerError().body("Server busy")
        }
    }
}


pub async fn create_driver(
    pool: web::Data<DbPool>,
    body: web::Json<DriverRequest>,
) -> HttpResponse {
    use crate::schema::drivers::dsl::*;

    let new_driver = NewDriver::new(body.into_inner());

    let result = web::block({
        let pool = pool.clone();
        move || -> Result<usize, String> {
            let mut conn = pool.get().map_err(|e| e.to_string())?;
            diesel::insert_into(drivers)
                .values(new_driver)
                .execute(&mut conn)
                .map_err(|e| e.to_string())
        }
    }).await;

    match result {
        Ok(Ok(_rows)) => HttpResponse::Ok().json("Driver created"),
        Ok(Err(db_err)) => HttpResponse::BadRequest().body(format!("DB error: {}", db_err)),
        Err(block_err) => HttpResponse::InternalServerError().body(format!("Threadpool error: {}", block_err)),
    }
}


pub async fn get_drivers(pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::drivers::dsl::*;

    let results = web::block({
        let pool = pool.clone();
        move || -> Result<Vec<Driver>, String> {
            let mut connection = pool.get().map_err(|e| e.to_string())?;
            drivers.limit(20)
                   .select(Driver::as_select())
                   .load::<Driver>(&mut connection)
                   .map_err(|e| e.to_string())
        }
    }).await;

    match results {
        Ok(Ok(data)) => HttpResponse::Ok().json(data),
        Ok(Err(db_err)) => {
            eprintln!("DB error: {:?}", db_err);
            HttpResponse::InternalServerError().body("Database error")
        }
        Err(blocking_err) => {
            eprintln!("Blocking error: {:?}", blocking_err);
            HttpResponse::InternalServerError().body("Server busy")
        }
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
    pub rider_pubkey: String,
}

#[derive(Queryable, Serialize, Deserialize, Selectable, Clone)]
#[diesel(table_name = crate::schema::riders)]
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
#[diesel(table_name = crate::schema::riders)]
pub struct NewRider {
    pub rider_id: Uuid,
    pub rider_pubkey: serde_json::Value,
    pub name: String,
    pub email: String,
    pub phone: String,
}

impl NewRider {
    pub fn new(req: RiderRequest) -> Self {
        Self {
            rider_id: Uuid::new_v4(),
            rider_pubkey: serde_json::to_value(&req.rider_pubkey).expect("serialize pubkey"),
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
    pub driver_pubkey: String,
    pub license_number: Option<String>,
    pub vehicle_type: String,
    pub vehicle: Option<String>,
}



#[derive(Insertable, Deserialize, Clone)]
#[diesel(table_name = crate::schema::drivers)]
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
}


impl NewDriver {
    pub fn new(req: DriverRequest) -> Self {
        Self {
            driver_id: Uuid::new_v4(),
            driver_pubkey: serde_json::to_value(&req.driver_pubkey).expect("serialize pubkey"),
            name: req.name,
            email: req.email,
            phone: req.phone,
            status: "available".to_string(),
            driver_location: serde_json::to_value(GeoPoint { lat: 0.0, lng: 0.0, name: Some("Earth".to_string()) }).expect("serialize geo"),
            license_number: req.license_number,
            vehicle_type: req.vehicle_type,
            driver_response: serde_json::to_value(DriverResponse::Timeout).expect("serialize driver response"),
            vehicle: req.vehicle,
        }
    }
}
