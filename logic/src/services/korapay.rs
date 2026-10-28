use actix_web::{ web, Scope, HttpResponse, http::StatusCode };
use reqwest::Client;
use serde::{ Deserialize, Serialize };
use serde_json::{ json, Value };
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::sql_types::{ BigInt, Bool, Integer, Nullable, Text };
use uuid::Uuid;
use std::env;
use std::time::Duration;
use crate::db::DbPool;
use crate::api::errors::{ ApiError, blocking, json_error };
use crate::api::orders::{ OrderKey, OrderRow, cancel_unpaid, close_trip, load_order, paid_amount, payout_failed };
use crate::api::riders::{ ItemDetails, RideType, OrderSource, quote, same_place, validate_items, validate_points, validate_rider_account };
use crate::services::auth::AuthUser;
use crate::services::{ codes, escrow };
use crate::services::areas::require_service_area;
use crate::services::pricing::GeoPoint;

// Korapay merchant API, amounts in naira (major units), currency NGN.
//   money in:  customer -> Korapay holding account        (initialize)
//   money out: holding account -> customer                (refund: /orders/cancel, abandoned no_driver sweep)
//              holding account -> driver's bank account   (credit_driver, from confirm-dropoff)
// The order itself is created by the app (create_order RPC, awaiting_payment); this file only
// attaches the payment to it.

// The driver's share of what the customer paid, in basis points:
//   trip on an investment asset (back_trips.asset_id): driver 55, asset account 14, treasury 31
//   any other trip:                                    driver 85, treasury 15
// Only the driver's share leaves per trip; the rest stays in the holding account.
const DRIVER_SHARE_BPS: i64 = 8500;
const ASSET_TRIP_DRIVER_SHARE_BPS: i64 = 5500;
// The asset account's share of an asset trip (the investors' pool); reported, not paid per trip.
pub const ASSET_POOL_BPS: i64 = 1400;

fn env_required(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("{} is not set", name))
}

// e.g. https://api.korapay.com/merchant/api/v1
fn base_url() -> Result<String, String> {
    env_required("KORAPAY_BASE_URL").map(|url| url.trim_end_matches('/').to_string())
}

fn secret_key() -> Result<String, String> {
    env_required("KORAPAY_SECRET_KEY")
}

fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct Envelope<T> {
    status: bool,
    #[serde(default)]
    message: Option<String>,
    data: Option<T>,
}

async fn read_envelope<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<(reqwest::StatusCode, Envelope<T>), String> {
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Korapay read failed: {}", e))?;
    let envelope = serde_json::from_str::<Envelope<T>>(&text)
        .map_err(|e| format!("Korapay replied {} with an unexpected body ({}): {}", status, e, text))?;
    Ok((status, envelope))
}

async fn post<T: for<'de> Deserialize<'de>>(path: &str, body: &Value) -> Result<(reqwest::StatusCode, Envelope<T>), String> {
    let (base, key) = (base_url()?, secret_key()?);
    let resp = client()?
        .post(format!("{}/{}", base, path))
        .bearer_auth(key)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Failed to reach Korapay: {}", e))?;
    read_envelope(resp).await
}

// Money going out is referenced from the payment, so a retry is the same transfer and Korapay
// won't send it twice. Korapay caps references at 50 characters.
pub(crate) fn derived_reference(prefix: &str, payment_reference: &str) -> String {
    let mut reference = format!("{}_{}", prefix, payment_reference);
    reference.truncate(50);
    reference
}

// ---------- Money in ----------

#[derive(Deserialize)]
struct CheckoutData {
    checkout_url: String,
}

// Orders here; investment slots in services::invest.
pub(crate) async fn initialize_charge(reference: &str, amount_naira: i64, email: &str, name: &str, redirect_url: &str, narration: &str, metadata: Value) -> Result<String, String> {
    let body = json!({
        "amount": amount_naira,
        "currency": "NGN",
        "reference": reference,
        "redirect_url": redirect_url,
        "narration": narration,
        "customer": { "email": email, "name": name },
        "metadata": metadata,
    });
    let (status, envelope) = post::<CheckoutData>("charges/initialize", &body).await?;
    match envelope.data {
        Some(data) if envelope.status && status.is_success() => Ok(data.checkout_url),
        _ => Err(format!("Korapay refused the charge ({}): {}", status, envelope.message.unwrap_or_default())),
    }
}

#[derive(Deserialize)]
pub struct InitializeRequest {
    // The awaiting_payment order the app created with create_order. Optional only so a missing
    // one gets a clear message instead of a JSON error.
    #[serde(default)]
    pub order_ref: Option<Uuid>,
    pub pick_up: GeoPoint,
    pub drop_off: GeoPoint,
    pub ride_type: RideType,
    // estimated_price from /riders/ride-quote; re-checked here.
    #[serde(alias = "estimated_price")]
    pub price: i64,
    // The items the quote was for; their package types are part of the price.
    #[serde(default)]
    pub items: Vec<ItemDetails>,
    // Where Korapay sends the customer after the payment window.
    pub redirect_url: String,
}

#[derive(Serialize)]
pub struct InitializeResponse {
    pub checkout_url: String,
    // Korapay reference; /payments/korapay/cancel takes it.
    pub reference: String,
    pub order_ref: Uuid,
}

// The caller's own app order, still waiting for payment.
fn payable_order(conn: &mut PgConnection, order_ref: Uuid, me: Uuid) -> Result<OrderRow, ApiError> {
    let order = load_order(conn, OrderKey::Ref(order_ref), false)?
        .filter(|order| order.client_id == me)
        .ok_or_else(|| ApiError::NotFound("No order for this order_ref".into()))?;
    if order.source() != OrderSource::App {
        return Err(ApiError::BadRequest("Only app orders are paid here".into()));
    }
    if order.status != "awaiting_payment" {
        return Err(ApiError::Conflict(format!("Order is {}, not awaiting payment", order.status)));
    }
    Ok(order)
}

// Opens the payment window for an order the app created. Re-prices the trip and rejects a price
// that doesn't match, so only Rust's figure is ever charged, then writes the price, the
// reference and the pickup/drop-off codes onto the row. If it fails before that write the row
// stays awaiting_payment with no payment_reference (discard_unpaid_order, or the sweep).
// Calling it again replaces a reference whose charge never went through.
pub async fn initialize(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<InitializeRequest>,
) -> HttpResponse {
    let me = auth.user_id;
    let req = body.into_inner();
    let redirect_url = req.redirect_url.trim().to_string();
    if redirect_url.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "redirect_url is required");
    }
    if let Err(msg) = validate_points(&req.pick_up, &req.drop_off) {
        return json_error(StatusCode::BAD_REQUEST, msg);
    }

    let Some(order_ref) = req.order_ref else {
        return json_error(StatusCode::BAD_REQUEST, "order_ref is required: create the order (create_order) before paying");
    };
    let (pick_up, drop_off) = (req.pick_up.clone(), req.drop_off.clone());
    let loaded = blocking(&pool, move |conn| {
        require_service_area(conn, &pick_up, &drop_off)?;
        let order = payable_order(conn, order_ref, me)?;
        let rider = validate_rider_account(conn, me)
            .map_err(|_| ApiError::BadRequest("No rider account for this user".into()))?;
        Ok((order, rider))
    }).await;
    let (order, rider) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => return e.response(),
    };

    // The places being paid for are the ones on the order.
    if !same_place(order.pick_up(), &req.pick_up) || !same_place(order.drop_off(), &req.drop_off) {
        return json_error(StatusCode::CONFLICT, "pick_up or drop_off differs from the order");
    }
    if let Err(msg) = validate_items(&req.items) {
        return json_error(StatusCode::BAD_REQUEST, msg);
    }
    let quoted = quote(&req.pick_up, &req.drop_off, &req.ride_type, &req.items);
    let (price, insurance) = (quoted.estimated_price, quoted.insurance);
    if price != req.price {
        return json_error(StatusCode::CONFLICT, format!("Price has changed: this trip now costs {}; quote again", price));
    }

    // A reference already on the row must not be replaced while its money could still arrive.
    if let Some(previous) = order.payment_reference.as_deref() {
        match query_charge(previous).await {
            Ok(ChargeStatus::Success { .. }) =>
                return json_error(StatusCode::CONFLICT, "Order is already paid; send the ride request"),
            Ok(ChargeStatus::Processing) =>
                return json_error(StatusCode::CONFLICT, "A payment for this order is still processing"),
            Ok(ChargeStatus::Failed) | Ok(ChargeStatus::NotFound) => {},
            Err(e) => return ApiError::BadGateway(format!("Could not check the previous payment: {}", e)).response(),
        }
    }

    let reference = format!("asap_{}", Uuid::new_v4().simple());
    let metadata = json!({ "order_ref": order_ref, "order_id": order.id });
    let checkout_url = match initialize_charge(&reference, price, &rider.email, &rider.name, &redirect_url, "ASAP delivery", metadata).await {
        Ok(url) => url,
        Err(e) => return ApiError::BadGateway(format!("Could not start the payment: {}", e)).response(),
    };

    // Guarded on the reference it read, so two initialize calls can't both attach a charge.
    // Codes already on the row are kept (COALESCE), so the pair is written together or not at all.
    // insurance is the part of price that is package insurance.
    let previous = order.payment_reference.clone();
    let order_id = order.id;
    let (pickup_code, dropoff_code) = codes::code_pair();
    let written = blocking(&pool, {
        let reference = reference.clone();
        move |conn| Ok(diesel::sql_query(
            "UPDATE app_delivery_orders SET \
                payment_reference = $2, price = CAST($3 AS numeric), insurance = CAST($7 AS numeric), \
                pickup_code = COALESCE(pickup_code, $4), dropoff_code = COALESCE(dropoff_code, $5), \
                modified_at = now() \
             WHERE id = $1 AND status = 'awaiting_payment' AND payment_reference IS NOT DISTINCT FROM $6"
        )
        .bind::<BigInt, _>(order_id)
        .bind::<Text, _>(&reference)
        .bind::<BigInt, _>(price)
        .bind::<Text, _>(&pickup_code)
        .bind::<Text, _>(&dropoff_code)
        .bind::<Nullable<Text>, _>(previous.as_deref())
        .bind::<BigInt, _>(insurance)
        .execute(conn)?)
    }).await;

    match written {
        Ok(1) => HttpResponse::Ok().json(InitializeResponse { checkout_url, reference, order_ref }),
        // The checkout is never handed out, so nobody can pay it.
        Ok(_) => json_error(StatusCode::CONFLICT, "Order changed while starting the payment; try again"),
        Err(e) => e.response(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChargeStatus {
    // amount in naira, as Korapay charged it.
    Success { amount: f64 },
    Failed,
    Processing,
    // No charge under this reference (the payment window was never opened).
    NotFound,
}

#[derive(Deserialize)]
struct ChargeData {
    status: String,
    #[serde(default)]
    amount: Option<Value>,
    #[serde(default)]
    currency: Option<String>,
}

// Korapay sends amounts as numbers or numeric strings depending on the endpoint.
fn amount_from(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub async fn query_charge(reference: &str) -> Result<ChargeStatus, String> {
    let (base, key) = (base_url()?, secret_key()?);
    let resp = client()?
        .get(format!("{}/charges/{}", base, reference))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("Failed to reach Korapay: {}", e))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(ChargeStatus::NotFound);
    }

    let (status, envelope) = read_envelope::<ChargeData>(resp).await?;
    let data = match envelope.data {
        Some(data) if envelope.status && status.is_success() => data,
        _ => return Err(format!("Korapay charge lookup failed ({}): {}", status, envelope.message.unwrap_or_default())),
    };

    Ok(match data.status.as_str() {
        // Only naira counts as paid.
        "success" if data.currency.as_deref().map_or(true, |c| c.eq_ignore_ascii_case("NGN")) =>
            ChargeStatus::Success { amount: amount_from(data.amount.as_ref()).unwrap_or(0.0) },
        "success" | "failed" | "expired" | "cancelled" => ChargeStatus::Failed,
        _ => ChargeStatus::Processing,
    })
}

#[derive(Deserialize)]
pub struct CancelCheckoutRequest {
    pub reference: String,
}

#[derive(Serialize)]
pub struct CancelCheckoutResponse {
    pub order_ref: Option<Uuid>,
    // paid: the payment went through and the order stands (send the ride request);
    // cancelled: the order is closed.
    pub status: &'static str,
}

// The customer left an unfinished checkout. Korapay decides: a payment that already went through
// keeps the order, anything else cancels it. Never refunds; that is /orders/cancel.
pub async fn cancel_checkout(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<CancelCheckoutRequest>,
) -> HttpResponse {
    let me = auth.user_id;
    let reference = body.into_inner().reference.trim().to_string();
    if reference.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "reference is required");
    }

    let order = blocking(&pool, {
        let reference = reference.clone();
        move |conn| Ok(load_order(conn, OrderKey::PaymentReference(&reference), false)?)
    }).await;
    let order = match order {
        Ok(Some(order)) if order.client_id == me => order,
        Ok(_) => return json_error(StatusCode::NOT_FOUND, "No order for this reference"),
        Err(e) => return e.response(),
    };

    let order_ref = order.order_ref;
    let respond = |status| HttpResponse::Ok().json(CancelCheckoutResponse { order_ref, status });
    match order.status.as_str() {
        "awaiting_payment" => {},
        "cancelled" => return respond("cancelled"),
        _ => return respond("paid"),
    }

    match query_charge(&reference).await {
        Ok(ChargeStatus::Success { .. }) => return respond("paid"),
        // Cancelling now could leave a payment that lands later with no order.
        Ok(ChargeStatus::Processing) =>
            return json_error(StatusCode::CONFLICT, "The payment is still processing; try again in a moment"),
        Ok(_) => {},
        Err(e) => return ApiError::BadGateway(format!("Could not check the payment: {}", e)).response(),
    }

    let order_id = order.id;
    let status = blocking(&pool, move |conn| {
        if cancel_unpaid(conn, order_id, Some(me))? {
            return Ok("cancelled");
        }
        // Moved on first (a ride request opened it, or another cancel closed it).
        let order = load_order(conn, OrderKey::Id(order_id), false)?
            .ok_or_else(|| ApiError::NotFound("Order not found".into()))?;
        Ok(if order.status == "cancelled" { "cancelled" } else { "paid" })
    }).await;

    match status {
        Ok(status) => respond(status),
        Err(e) => e.response(),
    }
}

// ---------- Money out ----------

// Where a refund or payout Korapay was asked for stands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransferStatus {
    Success,
    Processing,
    Failed,
    // Korapay has nothing under this reference: it was never sent, or was refused outright.
    NotFound,
}

#[derive(Deserialize)]
struct TransferData {
    status: String,
}

async fn query_transfer(path: &str, reference: &str) -> Result<TransferStatus, String> {
    let (base, key) = (base_url()?, secret_key()?);
    let resp = client()?
        .get(format!("{}/{}/{}", base, path, reference))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("Failed to reach Korapay: {}", e))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(TransferStatus::NotFound);
    }

    let (status, envelope) = read_envelope::<TransferData>(resp).await?;
    let data = match envelope.data {
        Some(data) if envelope.status && status.is_success() => data,
        _ => return Err(format!("Korapay lookup of {} failed ({}): {}", reference, status, envelope.message.unwrap_or_default())),
    };
    Ok(match data.status.as_str() {
        "success" => TransferStatus::Success,
        "failed" | "reversed" => TransferStatus::Failed,
        _ => TransferStatus::Processing,
    })
}

// A driver payout, by its dr_ reference.
pub async fn query_payout(reference: &str) -> Result<TransferStatus, String> {
    query_transfer("transactions", reference).await
}

// A refund, by its rf_ reference.
pub(crate) async fn query_refund(reference: &str) -> Result<TransferStatus, String> {
    query_transfer("refunds", reference).await
}

#[derive(Debug, PartialEq)]
pub enum RefundOutcome {
    // Korapay took the refund; naira.
    Initiated(i64),
    // A refund under this payment's rf_ reference already exists.
    AlreadyRefunded,
    // Korapay holds no successful charge for the reference.
    NothingPaid,
}

// Refunds whatever Korapay charged under `payment_reference`, unless it was already refunded.
// For orders closed with no rider: a late payment on a cancelled checkout, a failed refund
// being retried.
pub async fn refund_if_paid(payment_reference: &str, reason: &str) -> Result<RefundOutcome, String> {
    match query_refund(&derived_reference("rf", payment_reference)).await? {
        TransferStatus::Success | TransferStatus::Processing => return Ok(RefundOutcome::AlreadyRefunded),
        TransferStatus::Failed | TransferStatus::NotFound => {},
    }
    let amount = match query_charge(payment_reference).await? {
        ChargeStatus::Success { amount } => amount.floor() as i64,
        _ => return Ok(RefundOutcome::NothingPaid),
    };
    if amount <= 0 {
        return Ok(RefundOutcome::NothingPaid);
    }
    refund(payment_reference, amount, reason).await?;
    Ok(RefundOutcome::Initiated(amount))
}

// Back to the customer: "Cancel & refund", or a no_driver order left untouched (sweep).
pub async fn refund(payment_reference: &str, amount_naira: i64, reason: &str) -> Result<(), String> {
    let body = json!({
        "payment_reference": payment_reference,
        "reference": derived_reference("rf", payment_reference),
        "amount": amount_naira,
        "reason": reason,
    });
    let (status, envelope) = post::<Value>("refunds/initiate", &body).await?;
    if envelope.status && status.is_success() {
        Ok(())
    } else {
        Err(format!("Korapay refused the refund ({}): {}", status, envelope.message.unwrap_or_default()))
    }
}

// Holding account -> bank account. Drivers here; investors' withdrawals in services::invest.
pub(crate) async fn disburse(reference: &str, amount_naira: f64, bank_code: &str, account_number: &str, name: &str, email: &str, narration: &str) -> Result<(), String> {
    let body = json!({
        "reference": reference,
        "destination": {
            "type": "bank_account",
            "amount": amount_naira,
            "currency": "NGN",
            "narration": narration,
            "bank_account": { "bank": bank_code, "account": account_number },
            "customer": { "name": name, "email": email },
        },
    });
    let (status, envelope) = post::<Value>("transactions/disburse", &body).await?;
    if envelope.status && status.is_success() {
        Ok(())
    } else {
        Err(format!("Korapay refused the payout ({}): {}", status, envelope.message.unwrap_or_default()))
    }
}

fn to_kobo(naira: f64) -> i64 {
    (naira * 100.0).round() as i64
}

// The driver's cut of what was paid, in kobo, rounded down. Whole-number maths, so the
// split never drifts by a kobo.
fn driver_share_kobo(paid_kobo: i64, asset_trip: bool) -> i64 {
    paid_kobo * driver_share_bps(asset_trip) / 10_000
}

// Also sent to Zazu, which pays its own drivers by the same split.
pub fn driver_share_bps(asset_trip: bool) -> i64 {
    if asset_trip { ASSET_TRIP_DRIVER_SHARE_BPS } else { DRIVER_SHARE_BPS }
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CreditStatus {
    // Korapay confirmed the money reached the driver's bank account; the trip is Completed.
    Paid,
    // Sent; Korapay hasn't confirmed it yet. The sweep keeps checking and completes the trip.
    Processing,
    // Zazu pays its own drivers.
    NotApplicable,
    // Not paid; `error` says why. Crediting the same order again is safe.
    Failed,
}

#[derive(Serialize)]
pub struct DriverCredit {
    pub status: CreditStatus,
    // Naira, to the kobo.
    pub amount: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DriverCredit {
    fn failed(amount: f64, error: impl Into<String>) -> Self {
        DriverCredit { status: CreditStatus::Failed, amount, error: Some(error.into()) }
    }
}

struct CreditTarget {
    order_ref: Option<Uuid>,
    payment_reference: String,
    order_code: String,
    name: String,
    email: String,
    account_number: Option<String>,
    bank_code: Option<String>,
    asset_trip: bool,
}

// Pays the driver their share of a delivered app order, from what the customer actually paid,
// and completes the trip once Korapay confirms the payout. Korapay is asked first, so a payout
// already sent is never sent again; only one Korapay has no record of is (re)sent. Called by
// confirm-dropoff, its follow-up checks and the sweep.
pub async fn credit_driver(pool: &web::Data<DbPool>, order_id: i64) -> DriverCredit {
    let target = blocking(pool, move |conn| {
        use crate::schema::app_delivery_orders::dsl as o;
        use crate::schema::back_drivers::dsl as d;

        let order = load_order(conn, OrderKey::Id(order_id), false)?
            .ok_or_else(|| ApiError::NotFound("Order not found".into()))?;
        if order.source() == OrderSource::Zazu {
            return Ok(None);
        }
        if order.status != "delivered" {
            return Err(ApiError::Conflict("Order is not delivered".into()));
        }
        let driver_id = order.driver_id.ok_or_else(|| ApiError::Conflict("Order has no driver".into()))?;
        let payment_reference = order.payment_reference
            .ok_or_else(|| ApiError::Conflict("Order has no payment_reference".into()))?;
        let order_code: String = o::app_delivery_orders.find(order_id).select(o::order_code).first(conn)?;
        let (name, email, account_number, bank_code) = d::back_drivers
            .find(driver_id)
            .select((d::name, d::email, d::account_number, d::bank_code))
            .first::<(String, String, Option<String>, Option<String>)>(conn)?;
        // Stamped at pickup and never changed, so a retried payout works out the same amount.
        let asset_trip = match order.order_ref {
            Some(order_ref) => {
                use crate::schema::back_trips::dsl as t;
                t::back_trips
                    .filter(t::order_ref.eq(order_ref))
                    .select(t::asset_id)
                    .first::<Option<Uuid>>(conn)
                    .optional()?
                    .flatten()
                    .is_some()
            }
            None => false,
        };

        Ok(Some(CreditTarget { order_ref: order.order_ref, payment_reference, order_code, name, email, account_number, bank_code, asset_trip }))
    }).await;

    let target = match target {
        Ok(Some(target)) => target,
        Ok(None) => return DriverCredit { status: CreditStatus::NotApplicable, amount: 0.0, error: None },
        Err(e) => return DriverCredit::failed(0.0, e.message()),
    };
    let paid = match query_charge(&target.payment_reference).await {
        Ok(ChargeStatus::Success { amount }) => amount,
        Ok(_) => return DriverCredit::failed(0.0, "The order's payment was not completed"),
        Err(e) => return DriverCredit::failed(0.0, e),
    };

    let share_kobo = driver_share_kobo(to_kobo(paid), target.asset_trip);
    let amount = share_kobo as f64 / 100.0;
    if share_kobo <= 0 {
        return DriverCredit::failed(0.0, "Driver share is zero");
    }

    let reference = derived_reference("dr", &target.payment_reference);
    let status = match query_payout(&reference).await {
        Ok(TransferStatus::NotFound) => {
            let (Some(account_number), Some(bank_code)) = (target.account_number.as_deref(), target.bank_code.as_deref()) else {
                return DriverCredit::failed(amount, "Driver bank details not set");
            };
            let narration = format!("ASAP delivery {}", target.order_code);
            if let Err(e) = disburse(&reference, amount, bank_code, account_number, &target.name, &target.email, &narration).await {
                eprintln!("Order {}: driver payout of {} failed: {}", order_id, amount, e);
                return DriverCredit::failed(amount, e);
            }
            TransferStatus::Processing
        }
        Ok(status) => status,
        Err(e) => return DriverCredit::failed(amount, e),
    };

    match status {
        TransferStatus::Success => {
            let order_ref = target.order_ref;
            match blocking(pool, move |conn| Ok(close_trip(conn, order_ref, "Completed")?)).await {
                // Only the call that completed the trip records it.
                Ok(true) => if let Some(order_ref) = order_ref {
                    escrow::spawn_record_ride(pool.clone(), order_ref, Some(reference));
                },
                Ok(false) => {},
                // Paid all the same; the sweep completes the trip on its next pass.
                Err(e) => eprintln!("Order {}: driver paid, trip not completed: {}", order_id, e.message()),
            }
            DriverCredit { status: CreditStatus::Paid, amount, error: None }
        }
        TransferStatus::Processing | TransferStatus::NotFound =>
            DriverCredit { status: CreditStatus::Processing, amount, error: None },
        TransferStatus::Failed => {
            // Korapay won't take this reference again, so it needs someone to look at it.
            let order_ref = target.order_ref;
            if let Err(e) = blocking(pool, move |conn| Ok(payout_failed(conn, order_ref)?)).await {
                eprintln!("Order {}: could not mark the trip's payout failed: {}", order_id, e.message());
            }
            eprintln!("Order {}: Korapay reports payout {} of {} failed", order_id, reference, amount);
            DriverCredit::failed(amount, "Korapay reports the payout failed; contact support")
        }
    }
}

// After confirm-dropoff sends a payout: a few quick checks, so the trip usually completes
// within minutes instead of waiting for the sweep.
const PAYOUT_CHECK_DELAYS_SECS: [u64; 3] = [15, 45, 120];

pub fn follow_payout(pool: web::Data<DbPool>, order_id: i64) {
    tokio::spawn(async move {
        for delay in PAYOUT_CHECK_DELAYS_SECS {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            if credit_driver(&pool, order_id).await.status != CreditStatus::Processing {
                return;
            }
        }
    });
}

// ---------- Stale-order sweep ----------

// awaiting_payment orders younger than this are left alone: the customer may still be paying.
const SWEEP_AFTER_MINS: i32 = 30;
// A charge still processing after this long is given up on.
const ABANDON_PROCESSING_MINS: i32 = 120;
// A paid order sitting in no_driver this long (no Try again, no Cancel) is closed and refunded.
const ABANDON_NO_DRIVER_MINS: i32 = 24 * 60;
const SWEEP_EVERY_SECS: u64 = 300;

#[derive(QueryableByName)]
struct StuckOrder {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Nullable<Text>)]
    payment_reference: Option<String>,
    #[diesel(sql_type = Bool)]
    abandoned: bool,
}

fn unpaid_orders(conn: &mut PgConnection) -> QueryResult<Vec<StuckOrder>> {
    diesel::sql_query(
        "SELECT id, payment_reference, created_at < now() - make_interval(mins => $2) AS abandoned \
         FROM app_delivery_orders \
         WHERE status = 'awaiting_payment' AND source = 'app' \
           AND created_at < now() - make_interval(mins => $1) \
         ORDER BY created_at \
         LIMIT 200"
    )
    .bind::<Integer, _>(SWEEP_AFTER_MINS)
    .bind::<Integer, _>(ABANDON_PROCESSING_MINS)
    .load(conn)
}

// modified_at is when the search marked it no_driver; a Try again moves it back to pending.
fn abandoned_no_driver_orders(conn: &mut PgConnection) -> QueryResult<Vec<StuckOrder>> {
    diesel::sql_query(
        "SELECT id, payment_reference, true AS abandoned \
         FROM app_delivery_orders \
         WHERE status = 'no_driver' AND source = 'app' AND driver_id IS NULL \
           AND modified_at < now() - make_interval(mins => $1) \
         ORDER BY modified_at \
         LIMIT 200"
    )
    .bind::<Integer, _>(ABANDON_NO_DRIVER_MINS)
    .load(conn)
}

// Whether the sweep may close an awaiting_payment order. Only unpaid ones: a paid order stays
// until the customer sends the ride request or cancels.
async fn unpaid(order: &StuckOrder) -> Result<bool, String> {
    let Some(reference) = order.payment_reference.as_deref() else {
        // initialize never succeeded, so there is no charge.
        return Ok(true);
    };
    Ok(match query_charge(reference).await? {
        ChargeStatus::Failed | ChargeStatus::NotFound => true,
        ChargeStatus::Processing => order.abandoned,
        ChargeStatus::Success { .. } => false,
    })
}

// Closes a no_driver order nobody came back to. Guarded on no_driver, so a Try again that
// reopened it first wins.
fn cancel_no_driver(conn: &mut PgConnection, order_id: i64) -> QueryResult<bool> {
    use crate::schema::app_delivery_orders::dsl as o;
    use crate::api::riders::{ delivery_status, db_now_nullable };

    let updated = diesel::update(
        o::app_delivery_orders
            .filter(o::id.eq(order_id))
            .filter(o::driver_id.is_null())
            .filter(o::status.eq(delivery_status("no_driver")))
    )
    .set((
        o::status.eq(delivery_status("cancelled")),
        o::cancelled_at.eq(db_now_nullable()),
        o::cancelled_by.eq(None::<Uuid>),
        o::modified_at.eq(diesel::dsl::now),
    ))
    .execute(conn)?;
    Ok(updated == 1)
}

async fn sweep_unpaid(pool: &web::Data<DbPool>) {
    let stuck = match blocking(pool, |conn| Ok(unpaid_orders(conn)?)).await {
        Ok(stuck) => stuck,
        Err(e) => return eprintln!("Payment sweep: could not load unpaid orders: {}", e.message()),
    };

    for order in stuck {
        match unpaid(&order).await {
            Ok(true) => {},
            Ok(false) => continue,
            Err(e) => {
                eprintln!("Payment sweep: order {}: {}", order.id, e);
                continue;
            }
        }
        let order_id = order.id;
        match blocking(pool, move |conn| Ok(cancel_unpaid(conn, order_id, None)?)).await {
            Ok(true) => println!("Payment sweep: order {} cancelled, never paid", order_id),
            Ok(false) => {},
            Err(e) => eprintln!("Payment sweep: order {}: could not cancel: {}", order_id, e.message()),
        }
    }
}

async fn sweep_no_driver(pool: &web::Data<DbPool>) {
    let stuck = match blocking(pool, |conn| Ok(abandoned_no_driver_orders(conn)?)).await {
        Ok(stuck) => stuck,
        Err(e) => return eprintln!("Payment sweep: could not load no_driver orders: {}", e.message()),
    };

    for order in stuck {
        let order_id = order.id;
        match blocking(pool, move |conn| Ok(cancel_no_driver(conn, order_id)?)).await {
            Ok(true) => {},
            Ok(false) => continue,
            Err(e) => {
                eprintln!("Payment sweep: order {}: could not cancel: {}", order_id, e.message());
                continue;
            }
        }
        println!("Payment sweep: order {} cancelled after {} minutes in no_driver", order_id, ABANDON_NO_DRIVER_MINS);

        // No rider was ever assigned, so whatever was paid goes back. A failed refund here is
        // retried by the customer's Cancel & refund (same reference, never paid twice).
        let Some(reference) = order.payment_reference.as_deref() else { continue };
        match paid_amount(reference).await {
            Ok(Some(amount)) if amount > 0 => {
                if let Err(e) = refund(reference, amount, "No rider was found for your delivery").await {
                    eprintln!("Payment sweep: order {}: refund of {} failed: {}", order_id, amount, e);
                }
            }
            Ok(_) => {},
            Err(e) => eprintln!("Payment sweep: order {}: could not check payment for refund: {}", order_id, e),
        }
    }
}

#[derive(QueryableByName)]
struct OrderId {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

// Delivered app orders whose trip is still waiting on Korapay to confirm the driver's payout.
fn awaiting_payout_orders(conn: &mut PgConnection) -> QueryResult<Vec<OrderId>> {
    diesel::sql_query(
        "SELECT o.id FROM back_trips t \
         JOIN app_delivery_orders o ON o.order_ref = t.order_ref \
         WHERE t.status = 'AwaitingPayout' AND o.status = 'delivered' AND o.source = 'app' \
         ORDER BY t.start_ts \
         LIMIT 200"
    )
    .load(conn)
}

async fn sweep_payouts(pool: &web::Data<DbPool>) {
    let waiting = match blocking(pool, |conn| Ok(awaiting_payout_orders(conn)?)).await {
        Ok(waiting) => waiting,
        Err(e) => return eprintln!("Payment sweep: could not load unconfirmed payouts: {}", e.message()),
    };
    for order in waiting {
        let credit = credit_driver(pool, order.id).await;
        match credit.status {
            CreditStatus::Paid => println!("Payment sweep: order {} payout confirmed, trip completed", order.id),
            CreditStatus::Failed => eprintln!("Payment sweep: order {}: payout: {}", order.id, credit.error.unwrap_or_default()),
            CreditStatus::Processing | CreditStatus::NotApplicable => {},
        }
    }
}

// A cancelled order is watched this long for a payment that lands after the cancel (a checkout
// left open, a refund that failed).
const LATE_PAYMENT_WINDOW_MINS: i32 = 6 * 60;

// Cancelled app orders that never had a rider but did get a payment reference.
fn recently_cancelled_unassigned(conn: &mut PgConnection) -> QueryResult<Vec<StuckOrder>> {
    diesel::sql_query(
        "SELECT id, payment_reference, true AS abandoned \
         FROM app_delivery_orders \
         WHERE status = 'cancelled' AND source = 'app' AND driver_id IS NULL \
           AND payment_reference IS NOT NULL \
           AND cancelled_at > now() - make_interval(mins => $1) \
         ORDER BY cancelled_at \
         LIMIT 200"
    )
    .bind::<Integer, _>(LATE_PAYMENT_WINDOW_MINS)
    .load(conn)
}

async fn sweep_late_payments(pool: &web::Data<DbPool>) {
    let cancelled = match blocking(pool, |conn| Ok(recently_cancelled_unassigned(conn)?)).await {
        Ok(cancelled) => cancelled,
        Err(e) => return eprintln!("Payment sweep: could not load cancelled orders: {}", e.message()),
    };
    for order in cancelled {
        let Some(reference) = order.payment_reference.as_deref() else { continue };
        match refund_if_paid(reference, "Your order was cancelled before a rider was assigned").await {
            Ok(RefundOutcome::Initiated(amount)) =>
                println!("Payment sweep: order {} was paid after it closed; refunded {}", order.id, amount),
            Ok(_) => {},
            Err(e) => eprintln!("Payment sweep: order {}: late-payment refund: {}", order.id, e),
        }
    }
}

// Runs the sweep for the life of the server.
pub fn spawn_stale_order_sweep(pool: web::Data<DbPool>) {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(SWEEP_EVERY_SECS));
        loop {
            ticks.tick().await;
            sweep_unpaid(&pool).await;
            sweep_no_driver(&pool).await;
            sweep_late_payments(&pool).await;
            sweep_payouts(&pool).await;
            crate::api::disputes::sweep_dispute_refunds(&pool).await;
            crate::services::sessions::sweep_sessions(&pool).await;
        }
    });
}

// ---------- Banks ----------

#[derive(Deserialize, Serialize, Clone)]
pub struct Bank {
    pub name: String,
    // Korapay's bank code: what a driver saves through update-bank-details and payouts send to.
    pub code: String,
}

const BANKS_CACHE_SECS: u64 = 24 * 60 * 60;

lazy_static::lazy_static! {
    static ref BANKS_CACHE: tokio::sync::Mutex<Option<(std::time::Instant, Vec<Bank>)>> =
        tokio::sync::Mutex::new(None);
}

// Korapay documents this lookup with the public key; the secret key is used when none is set.
async fn fetch_banks() -> Result<Vec<Bank>, String> {
    let base = base_url()?;
    let key = env_required("KORAPAY_PUBLIC_KEY").or_else(|_| secret_key())?;
    let resp = client()?
        .get(format!("{}/misc/banks", base))
        .query(&[("countryCode", "NG")])
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("Failed to reach Korapay: {}", e))?;
    let (status, envelope) = read_envelope::<Vec<Bank>>(resp).await?;
    match envelope.data {
        Some(banks) if envelope.status && status.is_success() && !banks.is_empty() => Ok(banks),
        _ => Err(format!("Korapay refused the bank list ({}): {}", status, envelope.message.unwrap_or_default())),
    }
}

// Nigerian banks for the driver's bank details form, as [{name, code}]. Public: the list is the
// same for everyone. Cached for a day; if Korapay is down, a stale list is served over none.
pub async fn list_banks() -> HttpResponse {
    let mut cache = BANKS_CACHE.lock().await;
    if let Some((at, banks)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(BANKS_CACHE_SECS) {
            return HttpResponse::Ok().json(banks);
        }
    }
    match fetch_banks().await {
        Ok(banks) => {
            *cache = Some((std::time::Instant::now(), banks.clone()));
            HttpResponse::Ok().json(banks)
        }
        Err(e) => match cache.as_ref() {
            Some((_, banks)) => {
                eprintln!("Bank list refresh failed, serving the cached one: {}", e);
                HttpResponse::Ok().json(banks)
            }
            None => ApiError::BadGateway(format!("Could not load banks: {}", e)).response(),
        },
    }
}

// Its own resource rather than a /payments scope, which would shadow /payments/korapay.
pub fn banks_route() -> actix_web::Resource {
    web::resource("/payments/banks").route(web::get().to(list_banks))
}

pub fn routes() -> Scope {
    web::scope("/payments/korapay")
        .route("/initialize", web::post().to(initialize))
        .route("/cancel", web::post().to(cancel_checkout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_references_fit_korapay() {
        let long = "asap_".to_string() + &"a".repeat(60);
        assert_eq!(derived_reference("rf", &long).len(), 50);
        assert_eq!(derived_reference("dr", "asap_x"), "dr_asap_x");
    }

    #[test]
    fn reads_both_amount_shapes() {
        assert_eq!(amount_from(Some(&json!(1500))), Some(1500.0));
        assert_eq!(amount_from(Some(&json!("1500.50"))), Some(1500.5));
        assert_eq!(amount_from(None), None);
    }

    #[test]
    fn driver_share_rounds_down_to_the_kobo() {
        assert_eq!(to_kobo(1999.0), 199_900);
        assert_eq!(driver_share_kobo(199_900, false), 169_915); // 85%: ₦1,699.15
        assert_eq!(driver_share_kobo(199_900, true), 109_945);  // 55%: ₦1,099.45
        assert_eq!(driver_share_kobo(100_000, false), 85_000);
        assert_eq!(driver_share_kobo(100_000, true), 55_000);
        assert_eq!(driver_share_kobo(1, false), 0);
    }
}
