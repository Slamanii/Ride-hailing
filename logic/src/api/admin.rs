use actix_web::{ get, post, web, Scope, HttpResponse,  };
use serde::{ Deserialize, Serialize };
use serde_json::json;
use diesel::prelude::*;
use crate::db::{ DbPool };
use crate::services::pricing::{ GeoPoint };
use uuid::Uuid;
use solana_sdk::pubkey::Pubkey;
use crate::api::drivers::{ Driver, DriverResponse };

pub async fn admin_dashboard(pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::riders::dsl::*;
    use crate::schema::drivers::dsl::*;

    // Count riders
    let rider_count_result: Result<i64, _> = web::block({
        let pool = pool.clone();
        move || {
            let mut conn = pool.get().expect("Failed to get DB connection");
            riders.count()
                  .get_result(&mut conn)
        }
    }).await.expect("Failed to await rider count");

    let rider_count = match rider_count_result {
        Ok(count) => count,
        Err(_) => return HttpResponse::InternalServerError().body("Error counting riders"),
    };

    // Count drivers
    let driver_count_result: Result<i64, _> = web::block({
        let pool = pool.clone();
        move || {
            let mut conn = pool.get().expect("Failed to get DB connection");
            drivers.count()
                   .get_result(&mut conn)
        }
    }).await.expect("Failed to await driver count");

    let driver_count = match driver_count_result {
        Ok(count) => count,
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
        
        move || {
        let mut conn = pool.get().expect("Failed to get DB");
            
        diesel::insert_into(riders)
                .values(new_rider)
                .execute(&mut conn)
    
            }
        }).await;
            
            match result {
                Ok(_) => HttpResponse::Ok().json("Driver created"),
                Err(err) => HttpResponse::InternalServerError().body(format!("DB Error: {:?}", err)),
    }
}

pub async fn get_riders(pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::riders::dsl::*;

    let results = web::block({
        
        let pool = pool.clone();
        move || {

        let mut connection = pool.get().expect("Failed to get DB connection");

        riders.limit(20)
              .select(Rider::as_select())
              .load::<Rider>(&mut connection)
        }
    }).await;

     match results {
    Ok(Ok(data)) => HttpResponse::Ok().json(data),

    Ok(Err(db_err)) => {
        eprintln!("DB error: {:?}", db_err);
        HttpResponse::InternalServerError()
            .body("Database error")
    }

    Err(blocking_err) => {
        eprintln!("Blocking error: {:?}", blocking_err);
        HttpResponse::InternalServerError()
            .body("Server busy")
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
        
        move || {
        let mut conn = pool.get().expect("Failed to get DB");

        diesel::insert_into(drivers)
            .values(new_driver)
            .execute(&mut conn)
        }
    }).await;

    match result {
        Ok(_) => HttpResponse::Ok().json("Driver created"),
        Err(err) => HttpResponse::InternalServerError().body(format!("DB Error: {:?}", err)),
    }
}


pub async fn get_drivers(pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::drivers::dsl::*;

    let results = web::block({

        let pool = pool.clone();
        
        move || {
        
        let mut connection = pool.get().expect("Failed to get DB connection");

        drivers.limit(20)
                .select(Driver::as_select())
                .load::<Driver>(&mut connection)
        }
    }).await;

        match results {
    Ok(Ok(data)) => HttpResponse::Ok().json(data),

    Ok(Err(db_err)) => {
        eprintln!("DB error: {:?}", db_err);
        HttpResponse::InternalServerError()
            .body("Database error")
    }

    Err(blocking_err) => {
        eprintln!("Blocking error: {:?}", blocking_err);
        HttpResponse::InternalServerError()
            .body("Server busy")
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
}

#[derive(Queryable, Serialize, Deserialize, Selectable, Clone)]
#[diesel(table_name = crate::schema::riders)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Rider {
    pub rider_id: Uuid,
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
            rider_pubkey: serde_json::to_value(Pubkey::new_unique().to_string()).expect("serialize pubkey"),
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
            driver_pubkey: serde_json::to_value(Pubkey::new_unique().to_string()).expect("serialize pubkey"),
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