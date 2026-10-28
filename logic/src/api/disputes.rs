use actix_web::{ web, HttpResponse };
use serde::{ Deserialize, Serialize };
use serde_json::json;
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::sql_types::{ BigInt, Bool, Nullable, Text, Timestamptz, Uuid as SqlUuid };
use chrono::{ DateTime, Utc };
use uuid::Uuid;
use crate::api::errors::{ ApiError, blocking };
use crate::api::drivers::{ drivers_order, refresh_driver_status, require_near };
use crate::api::orders::{ close_trip, expire_open_offers };
use crate::api::riders::OrderSource;
use crate::db::DbPool;
use crate::services::auth::{ AdminUser, AuthUser };
use crate::services::{ korapay, zazu };

// Package disputes. At the pickup a driver can refuse a package that isn't what the order
// said (over 35kg, too big, prohibited, missing, wrong place). The order is cancelled on the
// spot and nothing is refunded yet: the whole charge waits in the holding account for
// management's review. The customer can see the dispute and contact support, but nothing
// they do changes it.
//   upheld    the driver was right: they earn DRIVER_SHARE_PCT of the charge, the customer is
//             refunded the rest
//   rejected  the driver was at fault: the customer is refunded in full and the driver gets
//             the penalty the reviewer sets
// What the driver earns or owes goes on back_driver_adjustments, netted once a month.
// Zazu orders: Zazu holds the money, so nothing is refunded or compensated here; a penalty
// still applies.

const DRIVER_SHARE_PCT: i64 = 20;

const REASONS: [&str; 5] = [
    "dimensions_exceeded", "weight_exceeded", "prohibited_contents", "package_not_found", "wrong_location",
];

#[derive(Deserialize)]
pub struct ReportPackageIssue {
    // order_ref UUID or the order_code.
    pub order_ref: String,
    pub reason: String,
    // Required: the reviewer decides from it.
    pub photo_url: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(QueryableByName)]
struct NewDispute {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

// POST /drivers/report-package-issue: the driver, at the pickup, before entering the pickup
// code. Cancels the order and frees the driver.
pub async fn report_package_issue(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<ReportPackageIssue>,
) -> HttpResponse {
    let me = auth.user_id;
    let req = body.into_inner();
    let reason = req.reason.trim().to_string();
    if !REASONS.contains(&reason.as_str()) {
        return ApiError::BadRequest(format!("reason must be one of: {}", REASONS.join(", "))).response();
    }
    let photo_url = req.photo_url.trim().to_string();
    if photo_url.is_empty() {
        return ApiError::BadRequest("photo_url is required".into()).response();
    }
    let note = req.note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());

    let result = blocking(&pool, move |conn| conn.transaction(|conn| {
        let order = drivers_order(conn, &req.order_ref, me, true)?;
        match order.status.as_str() {
            "arriving_pickup" | "driver_arrived" if !order.is_pickup_code_authenticated => {},
            "cancelled" => return Err(ApiError::Conflict("Order is already cancelled".into())),
            other => return Err(ApiError::Conflict(format!(
                "Order is {}; a package can only be disputed at the pickup, before the pickup code", other
            ))),
        }
        require_near(conn, me, order.pick_up(), "pickup")?;

        let dispute: NewDispute = diesel::sql_query(
            "INSERT INTO back_package_disputes (order_id, driver_id, reason, photo_url, driver_note) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id"
        )
        .bind::<BigInt, _>(order.id)
        .bind::<SqlUuid, _>(me)
        .bind::<Text, _>(&reason)
        .bind::<Text, _>(&photo_url)
        .bind::<Nullable<Text>, _>(&note)
        .get_result(conn)?;

        diesel::sql_query(
            "UPDATE app_delivery_orders SET status = 'cancelled', cancelled_at = now(), cancelled_by = $2, \
                    modified_at = now() \
              WHERE id = $1"
        )
        .bind::<BigInt, _>(order.id)
        .bind::<SqlUuid, _>(me)
        .execute(conn)?;
        expire_open_offers(conn, order.id)?;
        close_trip(conn, order.order_ref, "Cancelled")?;
        refresh_driver_status(conn, me)?;
        Ok((dispute.id, order.id))
    })).await;

    match result {
        Ok((dispute_id, order_id)) => {
            zazu::spawn_notify_status(pool.clone(), order_id);
            HttpResponse::Created().json(json!({
                "dispute_id": dispute_id,
                "order_id": order_id,
                "status": "pending",
                "order_status": "cancelled",
            }))
        }
        Err(e) => e.response(),
    }
}

// ---------- Review (admin) ----------

#[derive(Deserialize)]
pub struct DisputeReview {
    pub driver_at_fault: bool,
    // Kobo; required when the driver is at fault (0 for a warning).
    #[serde(default)]
    pub penalty: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(QueryableByName)]
struct DisputeOrder {
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = SqlUuid)]
    driver_id: Uuid,
    #[diesel(sql_type = Text)]
    source: String,
    #[diesel(sql_type = Nullable<Text>)]
    payment_reference: Option<String>,
}

// How the held charge splits, kobo. Refunds go out in whole naira, so a refund is rounded down
// and the odd kobo stays in the treasury.
#[derive(Debug, PartialEq)]
struct Split {
    driver: i64,
    refund: i64,
}

fn split(paid_kobo: i64, driver_at_fault: bool) -> Split {
    let driver = if driver_at_fault { 0 } else { paid_kobo * DRIVER_SHARE_PCT / 100 };
    let refund = (paid_kobo - driver) / 100 * 100;
    Split { driver, refund }
}

// POST /admin/disputes/{id}/review. Once only; the reviewing admin is kept as reviewed_by.
pub async fn review_dispute(
    admin: AdminUser,
    pool: web::Data<DbPool>,
    path: web::Path<i64>,
    body: web::Json<DisputeReview>,
) -> HttpResponse {
    let dispute_id = path.into_inner();
    let reviewer = admin.user_id.to_string();
    let req = body.into_inner();
    let penalty = match (req.driver_at_fault, req.penalty) {
        (true, None) => return ApiError::BadRequest("penalty is required when the driver is at fault".into()).response(),
        (_, Some(p)) if p < 0 => return ApiError::BadRequest("penalty can't be negative".into()).response(),
        (true, Some(p)) => p,
        (false, _) => 0,
    };
    let note = req.note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());

    let found = blocking(&pool, move |conn| Ok(diesel::sql_query(
        "SELECT d.status, d.driver_id, o.source, o.payment_reference \
           FROM back_package_disputes d JOIN app_delivery_orders o ON o.id = d.order_id \
          WHERE d.id = $1"
    ).bind::<BigInt, _>(dispute_id).get_result::<DisputeOrder>(conn).optional()?)).await;
    let dispute = match found {
        Ok(Some(d)) if d.status == "pending" => d,
        Ok(Some(d)) => return ApiError::Conflict(format!("Dispute {} is already {}", dispute_id, d.status)).response(),
        Ok(None) => return ApiError::NotFound(format!("Dispute {} not found", dispute_id)).response(),
        Err(e) => return e.response(),
    };

    // What the customer actually paid; Zazu's money isn't ours to split.
    let zazu = dispute.source.eq_ignore_ascii_case(OrderSource::Zazu.as_str());
    let paid_kobo = match dispute.payment_reference.as_deref().filter(|_| !zazu) {
        Some(reference) => match korapay::query_charge(reference).await {
            Ok(korapay::ChargeStatus::Success { amount }) => (amount * 100.0).round() as i64,
            Ok(_) => 0,
            Err(e) => return ApiError::BadGateway(format!("Could not check the payment: {}", e)).response(),
        },
        None => 0,
    };
    let Split { driver, refund } = split(paid_kobo, req.driver_at_fault);
    let status = if req.driver_at_fault { "rejected" } else { "upheld" };
    let driver_id = dispute.driver_id;

    let decided = blocking(&pool, move |conn| conn.transaction(|conn| {
        let updated = diesel::sql_query(
            "UPDATE back_package_disputes \
                SET status = $2, paid_amount = $3, driver_amount = $4, refund_amount = $5, \
                    refund_status = CASE WHEN $5 > 0 THEN 'pending' ELSE 'none' END, \
                    penalty = $6, review_note = $7, reviewed_by = $8, reviewed_at = now() \
              WHERE id = $1 AND status = 'pending'"
        )
        .bind::<BigInt, _>(dispute_id)
        .bind::<Text, _>(status)
        .bind::<BigInt, _>(paid_kobo)
        .bind::<BigInt, _>(driver)
        .bind::<BigInt, _>(refund)
        .bind::<Nullable<BigInt>, _>(req.driver_at_fault.then_some(penalty))
        .bind::<Nullable<Text>, _>(note)
        .bind::<Text, _>(reviewer)
        .execute(conn)?;
        if updated == 0 {
            return Err(ApiError::Conflict(format!("Dispute {} was reviewed by someone else", dispute_id)));
        }

        let adjustment = if driver > 0 {
            Some((driver, "dispute_compensation"))
        } else if penalty > 0 {
            Some((-penalty, "dispute_penalty"))
        } else {
            None
        };
        if let Some((amount, kind)) = adjustment {
            diesel::sql_query(
                "INSERT INTO back_driver_adjustments (driver_id, amount, kind, dispute_id) VALUES ($1, $2, $3, $4)"
            )
            .bind::<SqlUuid, _>(driver_id)
            .bind::<BigInt, _>(amount)
            .bind::<Text, _>(kind)
            .bind::<BigInt, _>(dispute_id)
            .execute(conn)?;
        }
        Ok(())
    })).await;
    if let Err(e) = decided {
        return e.response();
    }

    let refund_status = if refund > 0 { send_refund(&pool, dispute_id).await } else { "none".to_string() };
    HttpResponse::Ok().json(json!({
        "dispute_id": dispute_id,
        "status": status,
        "paid_amount": paid_kobo,
        "driver_amount": driver,
        "refund_amount": refund,
        "refund_status": refund_status,
        "penalty": req.driver_at_fault.then_some(penalty),
    }))
}

#[derive(QueryableByName)]
struct RefundDue {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    payment_reference: String,
    #[diesel(sql_type = BigInt)]
    refund_amount: i64,
    #[diesel(sql_type = Bool)]
    driver_at_fault: bool,
}

// Sends a reviewed dispute's refund, unless Korapay already has it (same rf_ reference, so it
// is never paid twice). Returns the refund_status recorded.
async fn send_refund(pool: &web::Data<DbPool>, dispute_id: i64) -> String {
    let due = blocking(pool, move |conn| Ok(diesel::sql_query(
        "SELECT d.id, o.payment_reference, d.refund_amount, d.status = 'rejected' AS driver_at_fault \
           FROM back_package_disputes d JOIN app_delivery_orders o ON o.id = d.order_id \
          WHERE d.id = $1 AND d.refund_status IN ('pending', 'failed') AND o.payment_reference IS NOT NULL"
    ).bind::<BigInt, _>(dispute_id).get_result::<RefundDue>(conn).optional()?)).await;
    let due = match due {
        Ok(Some(due)) => due,
        Ok(None) => return "none".into(),
        Err(e) => {
            eprintln!("Dispute {}: refund not loaded: {}", dispute_id, e.message());
            return "pending".into();
        }
    };

    let already = korapay::query_refund(&korapay::derived_reference("rf", &due.payment_reference)).await;
    let outcome = match already {
        Ok(korapay::TransferStatus::Success) | Ok(korapay::TransferStatus::Processing) => Ok(()),
        Ok(_) => {
            let reason = if due.driver_at_fault {
                "Your order was cancelled at pickup; full refund after review"
            } else {
                "Your order was cancelled at pickup; refund after review"
            };
            korapay::refund(&due.payment_reference, due.refund_amount / 100, reason).await
        }
        Err(e) => Err(e),
    };

    let (status, error) = match outcome {
        Ok(()) => ("sent", None),
        Err(e) => {
            eprintln!("Dispute {}: refund failed: {}", due.id, e);
            ("failed", Some(e))
        }
    };
    let id = due.id;
    if let Err(e) = blocking(pool, move |conn| Ok(diesel::sql_query(
        "UPDATE back_package_disputes SET refund_status = $2, refund_error = $3 WHERE id = $1"
    ).bind::<BigInt, _>(id).bind::<Text, _>(status).bind::<Nullable<Text>, _>(error).execute(conn)?)).await {
        eprintln!("Dispute {}: refund status not saved: {}", id, e.message());
    }
    status.into()
}

#[derive(QueryableByName)]
struct DisputeId {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

// Retries refunds that failed or never went out (the reviewer's request dropped), from the
// stale-order sweep.
pub async fn sweep_dispute_refunds(pool: &web::Data<DbPool>) {
    let due = blocking(pool, |conn| Ok(diesel::sql_query(
        "SELECT id FROM back_package_disputes \
          WHERE refund_status IN ('pending', 'failed') AND reviewed_at < now() - interval '2 minutes' \
          ORDER BY reviewed_at LIMIT 100"
    ).load::<DisputeId>(conn)?)).await;
    match due {
        Ok(due) => for d in due { send_refund(pool, d.id).await; },
        Err(e) => eprintln!("Dispute sweep: could not load refunds due: {}", e.message()),
    }
}

// ---------- Review queue (admin) ----------

#[derive(QueryableByName, Serialize)]
pub struct DisputeListItem {
    #[diesel(sql_type = BigInt)]
    pub id: i64,
    #[diesel(sql_type = BigInt)]
    pub order_id: i64,
    #[diesel(sql_type = Text)]
    pub order_code: String,
    #[diesel(sql_type = Text)]
    pub source: String,
    #[diesel(sql_type = Text)]
    pub reason: String,
    #[diesel(sql_type = Text)]
    pub photo_url: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub driver_note: Option<String>,
    #[diesel(sql_type = SqlUuid)]
    pub driver_id: Uuid,
    #[diesel(sql_type = Text)]
    pub driver_name: String,
    #[diesel(sql_type = BigInt)]
    pub driver_disputes: i64,
    #[diesel(sql_type = Nullable<Text>)]
    pub package_description: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub package_type: Option<String>,
    #[diesel(sql_type = Text)]
    pub status: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub refund_status: Option<String>,
    #[diesel(sql_type = Timestamptz)]
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct DisputeFilter {
    // pending (default), upheld, rejected
    #[serde(default)]
    pub status: Option<String>,
}

// GET /admin/disputes?status=pending. driver_disputes: how many disputes that driver has
// raised in total, to spot one who uses them to skip orders.
pub async fn list_disputes(
    _admin: AdminUser,
    pool: web::Data<DbPool>,
    query: web::Query<DisputeFilter>,
) -> HttpResponse {
    let status = query.into_inner().status.unwrap_or_else(|| "pending".into());
    let result = blocking(&pool, move |conn| Ok(list(conn, &status)?)).await;
    match result {
        Ok(disputes) => HttpResponse::Ok().json(json!({ "disputes": disputes })),
        Err(e) => e.response(),
    }
}

fn list(conn: &mut PgConnection, status: &str) -> QueryResult<Vec<DisputeListItem>> {
    diesel::sql_query(
        "SELECT d.id, d.order_id, o.order_code, o.source, d.reason, d.photo_url, d.driver_note, \
                d.driver_id, dr.name AS driver_name, \
                (SELECT count(*) FROM back_package_disputes x WHERE x.driver_id = d.driver_id) AS driver_disputes, \
                o.package_description, o.package_type, d.status, d.refund_status, d.created_at \
           FROM back_package_disputes d \
           JOIN app_delivery_orders o ON o.id = d.order_id \
           JOIN back_drivers dr ON dr.driver_id = d.driver_id \
          WHERE d.status = $1 \
          ORDER BY d.created_at \
          LIMIT 200"
    )
    .bind::<Text, _>(status)
    .load(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upheld_dispute_pays_the_driver_a_fifth() {
        // ₦2,000 paid: driver ₦400, customer ₦1,600.
        assert_eq!(split(200_000, false), Split { driver: 40_000, refund: 160_000 });
        // ₦1,999.50: driver ₦399.90, customer refunded ₦1,599 (whole naira), 60 kobo stays.
        assert_eq!(split(199_950, false), Split { driver: 39_990, refund: 159_900 });
    }

    #[test]
    fn an_at_fault_driver_means_a_full_refund() {
        assert_eq!(split(200_000, true), Split { driver: 0, refund: 200_000 });
        assert_eq!(split(0, true), Split { driver: 0, refund: 0 });
    }
}
