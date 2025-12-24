use actix_web::{ web, post, Scope, HttpResponse };
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use crate::db::DbPool;
use crate::schema::trips::dsl::*;
use crate::api::trips;
use diesel::prelude::*;
use std::env;



pub async fn create_transaction(pool: web::Data<DbPool>, path: web::Path<String>) -> HttpResponse {
    
    let trip_reference = path.into_inner();

    let result = web::block({
        
        let pool = pool.clone();
        let trip_reference = trip_reference.clone();
        
        move || {

        let mut conn = pool.get().unwrap();        
        let trip = trips::get_trip_by_reference(&mut conn, &trip_reference)
                                               .map_err(|e| e.to_string());
        trip
        
        }   
    
    })
    .await;


    let trip = match result {

        Ok(Ok(trip)) => trip,
        Ok(Err(e)) => {
            println!("Trip error: {:?}", e);
            return HttpResponse::BadRequest().finish();
        }
        Err(block_err) => {
            println!("Threadpool error: {:?}", block_err);
            return HttpResponse::InternalServerError().finish();
        }
    };




    let client = Client::new();
    let secret_key = env::var("PAYSTACK_SECRET_KEY").expect("PAYSTACK_SECRET_KEY must be set");


    let fare_ngn = trip.fare_estimate.unwrap_or(0);
    let amount_kobo = fare_ngn * 100;
    let metadata = json!({
            "trip_id": trip.trip_id,
            "driver_id": trip.driver_id,
            "rider_id": trip.rider_id
        });


    let body = PaystackInitRequest {
        email: trip.rider_email.clone(),
        amount: amount_kobo as u64,
        reference: trip.reference.clone(),
        metadata,
    };

    let resp_result = client
    .post("https://api.paystack.co/transaction/initialize")
    .bearer_auth(secret_key)
    .json(&body)
    .send()
    .await;

            let resp = match resp_result {
                Ok(r) => r,
                Err(e) => {
                    println!("Failed to send request to Paystack: {:?}", e);
                    return HttpResponse::InternalServerError().body("Payment initialization failed");
                }
            };

            let json_result = resp.json::<PaystackInitResponse>().await;
            let json = match json_result {
                Ok(j) => j,
                Err(e) => {
                    println!("Failed to parse Paystack response: {:?}", e);
                    return HttpResponse::InternalServerError().body("Failed to parse payment response");
                }
            };


    // 6. Send auth_url back to frontend
    HttpResponse::Ok().json(serde_json::json!({
        "auth_url": json.data.authorization_url
    }))
}
  

pub fn routes() -> Scope {
    web::scope("/paystack")
        .route("/create-transaction/{reference}", web::post().to(create_transaction))

}


#[derive(Serialize)]
struct PaystackInitRequest {
    email: String,
    amount: u64,
    reference: String,
    metadata: serde_json::Value,
} 

#[derive(Deserialize)]
struct PaystackInitResponse {
    status: bool,
    message: String,
    data: PaystackInitData
}

#[derive(Deserialize)]
struct PaystackInitData {
    authorization_url: String,
    reference: String,
}

