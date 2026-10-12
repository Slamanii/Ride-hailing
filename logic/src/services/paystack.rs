use actix_web::{ web, Scope, HttpResponse };
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use crate::db::DbPool;
use crate::api::errors::{ ApiError, blocking };
use crate::api::trips;

// Legacy: app payments go through Korapay (/payments/korapay). Kept for manual use only.

pub async fn create_transaction(pool: web::Data<DbPool>, path: web::Path<String>) -> HttpResponse {
    let trip_reference = path.into_inner();

    let trip = blocking(&pool, move |conn| {
        trips::get_trip_by_reference(conn, &trip_reference)?
            .ok_or_else(|| ApiError::NotFound("Trip not found".into()))
    }).await;
    let trip = match trip {
        Ok(trip) => trip,
        Err(e) => return e.response(),
    };

    let secret_key = match env::var("PAYSTACK_SECRET_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => return ApiError::Internal("PAYSTACK_SECRET_KEY is not set".into()).response(),
    };

    let amount_kobo = trip.fare_estimate.unwrap_or(0).max(0) * 100;
    let body = PaystackInitRequest {
        email: trip.rider_email.clone(),
        amount: amount_kobo as u64,
        reference: trip.reference.clone(),
        metadata: json!({
            "trip_id": trip.trip_id,
            "driver_id": trip.driver_id,
            "rider_id": trip.rider_id
        }),
    };

    let resp = match Client::new()
        .post("https://api.paystack.co/transaction/initialize")
        .bearer_auth(secret_key)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => return ApiError::BadGateway(format!("Failed to reach Paystack: {}", e)).response(),
    };

    match resp.json::<PaystackInitResponse>().await {
        Ok(PaystackInitResponse { status: true, data: Some(data), .. }) =>
            HttpResponse::Ok().json(json!({ "auth_url": data.authorization_url })),
        Ok(other) => ApiError::BadGateway(format!("Paystack refused the transaction: {}", other.message)).response(),
        Err(e) => ApiError::BadGateway(format!("Unexpected Paystack response: {}", e)).response(),
    }
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
    #[serde(default)]
    message: String,
    data: Option<PaystackInitData>,
}

#[derive(Deserialize)]
struct PaystackInitData {
    authorization_url: String,
}
