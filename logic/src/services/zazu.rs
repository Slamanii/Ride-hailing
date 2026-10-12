use actix_web::web;
use diesel::prelude::*;
use diesel::sql_types::{ BigInt, Nullable, Text, Uuid as SqlUuid };
use reqwest::Client;
use std::env;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;
use crate::api::drivers::ZAZU_URL;
use crate::api::errors::blocking;
use crate::db::DbPool;

// Waits before each try; the first goes straight out.
const RETRY_DELAYS_SECS: [u64; 4] = [0, 2, 5, 15];

// POSTs to {ZAZU_URL}{path} with x-asap-secret. Network errors and 5xx are retried; a 4xx is
// not, since Zazu rejected the payload itself and sending it again won't change that.
pub async fn post(path: &str, payload: &serde_json::Value) -> Result<(), String> {
    let secret = env::var("ASAP_WEBHOOK_SECRET").unwrap_or_default();
    let client = Client::new();
    let mut last_error = String::new();
    for delay in RETRY_DELAYS_SECS {
        sleep(Duration::from_secs(delay)).await;
        // Read each time: Zazu may register its URL while this is retrying.
        let zazu_url = ZAZU_URL.lock().await.clone();
        if zazu_url.is_empty() {
            last_error = "Zazu URL not configured".into();
            continue;
        }
        match client
            .post(format!("{}{}", zazu_url, path))
            .header("x-asap-secret", &secret)
            .timeout(Duration::from_secs(10))
            .json(payload)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                let status = resp.status();
                last_error = format!("Zazu answered {}: {}", status, resp.text().await.unwrap_or_default());
                if status.is_client_error() {
                    break;
                }
            }
            Err(e) => last_error = format!("could not reach Zazu: {}", e),
        }
    }
    Err(last_error)
}

#[derive(QueryableByName)]
struct ZazuOrderStatus {
    #[diesel(sql_type = Nullable<SqlUuid>)]
    order_ref: Option<Uuid>,
    #[diesel(sql_type = Text)]
    order_code: String,
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    driver_id: Option<Uuid>,
    #[diesel(sql_type = Nullable<Text>)]
    updated_at: Option<String>,
}

// Tells Zazu where one of its orders stands: no_driver when the search ends without a driver,
// in_transit at pickup, delivered at drop-off, cancelled. App orders are skipped.
// The status is read when this runs, not passed in, so a late or repeated send carries the
// order's current state. Zazu should upsert by order_ref and ignore an updated_at older than
// the one it holds, since sends can arrive out of order.
// The driver's details go separately in /ride-details once a driver takes the order.
pub async fn notify_status(pool: web::Data<DbPool>, order_id: i64) {
    let order = blocking(&pool, move |conn| {
        Ok(diesel::sql_query(
            "SELECT order_ref, order_code, status::text AS status, driver_id, \
                to_char(modified_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS updated_at \
             FROM app_delivery_orders WHERE id = $1 AND lower(source) = 'zazu'"
        )
        .bind::<BigInt, _>(order_id)
        .get_result::<ZazuOrderStatus>(conn)
        .optional()?)
    }).await;

    let order = match order {
        Ok(Some(order)) => order,
        Ok(None) => return,
        Err(e) => return eprintln!("Order {}: status for Zazu not sent, could not load it: {}", order_id, e.message()),
    };

    let payload = serde_json::json!({
        "order_ref":  order.order_ref,
        "order_code": order.order_code,
        "status":     order.status,
        "driver_id":  order.driver_id,
        "updated_at": order.updated_at,
    });
    match post("/order-status", &payload).await {
        Ok(()) => println!("Order {}: Zazu told it is {}", order_id, order.status),
        Err(e) => eprintln!("Order {}: status {} for Zazu not delivered: {}", order_id, order.status, e),
    }
}

pub fn spawn_notify_status(pool: web::Data<DbPool>, order_id: i64) {
    tokio::spawn(notify_status(pool, order_id));
}
