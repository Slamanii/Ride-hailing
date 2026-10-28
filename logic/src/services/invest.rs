use actix_web::{ web, Scope, HttpResponse };
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::sql_types::{ BigInt, Nullable, Text, Uuid as SqlUuid };
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use crate::api::errors::{ ApiError, blocking };
use crate::db::DbPool;
use crate::services::auth::AuthUser;
use crate::services::korapay::{ self, TransferStatus };

// ASAP Invest, the Rust side of ASAP_APPS' invest.sql. Money is kobo, weeks start Monday,
// Lagos time.
//   weekly   each finished week is closed once: per asset, invest_weekly_stats; per slot owner,
//            invest_earnings (investor_pct of the fares, split by slot_pct) and a weekly_update.
//            The website shows these; invest_wallet (earned minus withdrawn) is the balance.
//   monthly  an investor asks for a withdrawal (one a month, up to their balance) and it is
//            paid at once from the Korapay holding account. Nothing is paid unasked.
// The fare counted is back_trips.fare_estimate of Completed asset trips, app and Zazu alike:
// what the customer was charged. An earning goes to whoever held the slot when the trip ended
// (invest_slot_owners), so a mid-week transfer splits the week. A slot the business holds earns
// nothing here; its share stays in the treasury.

const LAGOS: &str = "Africa/Lagos";

// Completed asset trips matching `filter`, with the fare in kobo and when the trip ended.
fn completed_asset_trips(filter: &str) -> String {
    format!(
        "SELECT encode(t.trip_id, 'hex') AS trip_id, t.asset_id, t.distance_km, \
                COALESCE(t.fare_estimate, 0) * 100 AS fare_kobo, to_timestamp(t.end_ts) AS done_at \
           FROM back_trips t \
          WHERE t.asset_id IS NOT NULL AND t.status = 'Completed' AND t.end_ts IS NOT NULL AND {}",
        filter
    )
}

// The Completed asset trips of week $1.
fn week_trips() -> String {
    completed_asset_trips(&format!(
        "to_timestamp(t.end_ts) >= ($1::date)::timestamp AT TIME ZONE '{tz}' \
         AND to_timestamp(t.end_ts) < ($1::date + 7)::timestamp AT TIME ZONE '{tz}'",
        tz = LAGOS
    ))
}

// invest_trip_earnings for the trips `trips` (a query on $1): one row per trip per investor
// who held a slot on the asset when the trip ended, owner_pct being all their slots summed.
// The owner's cut of a trip is fare * owner_pct / 100. Rows already written are left alone.
fn trip_earnings_insert(trips: &str) -> String {
    format!(
        "WITH trips AS ({trips}) \
         INSERT INTO invest_trip_earnings \
                (trip_id, asset_id, owner_id, week_start, trip_ended_at, distance_km, fare, owner_pct) \
         SELECT tr.trip_id, tr.asset_id, o.owner_id, date_trunc('week', tr.done_at AT TIME ZONE '{tz}')::date, \
                tr.done_at, round(tr.distance_km::numeric, 2), tr.fare_kobo, sum(s.slot_pct) \
           FROM trips tr \
           JOIN invest_slots s ON s.asset_id = tr.asset_id AND s.slot_pct IS NOT NULL \
           JOIN invest_slot_owners o ON o.slot_id = s.id AND o.holder = 'investor' \
            AND o.held_from <= tr.done_at AND (o.held_to IS NULL OR o.held_to > tr.done_at) \
          GROUP BY tr.trip_id, tr.asset_id, o.owner_id, tr.done_at, tr.distance_km, tr.fare_kobo \
         ON CONFLICT (trip_id, owner_id) DO NOTHING",
        trips = trips, tz = LAGOS
    )
}

// Called when a trip is completed (orders::close_trip). Runs in a savepoint and only logs a
// failure: completing the trip matters more, and the weekly close writes any row missed here.
pub fn record_trip_earnings(conn: &mut PgConnection, order_ref: uuid::Uuid) {
    let written = conn.transaction(|conn| {
        diesel::sql_query(trip_earnings_insert(&completed_asset_trips("t.order_ref = $1")))
            .bind::<SqlUuid, _>(order_ref)
            .execute(conn)
    });
    if let Err(e) = written {
        eprintln!("Invest: trip earnings for order_ref {} not written: {}", order_ref, e);
    }
}

#[derive(QueryableByName)]
struct WeekStart {
    #[diesel(sql_type = Text)]
    week_start: String,
}

// Finished weeks not closed yet, from the week the first asset went live. A week is left an
// hour past its end, for trips completing on Sunday night.
fn due_weeks(conn: &mut PgConnection) -> QueryResult<Vec<WeekStart>> {
    diesel::sql_query(format!(
        "SELECT w::date::text AS week_start \
           FROM generate_series( \
                  (SELECT date_trunc('week', min(activated_at AT TIME ZONE '{tz}')) FROM invest_assets), \
                  date_trunc('week', (now() - interval '1 hour') AT TIME ZONE '{tz}') - interval '7 days', \
                  interval '7 days') w \
          WHERE NOT EXISTS (SELECT 1 FROM back_invest_weeks b WHERE b.week_start = w::date) \
          ORDER BY w",
        tz = LAGOS
    ))
    .load(conn)
}

// Writes one week's stats, earnings and weekly_update notices. False if it was already closed.
fn close_week(conn: &mut PgConnection, week_start: &str) -> QueryResult<bool> {
    conn.transaction(|conn| {
        let claimed = diesel::sql_query(
            "INSERT INTO back_invest_weeks (week_start) VALUES ($1::date) ON CONFLICT DO NOTHING"
        )
        .bind::<Text, _>(week_start)
        .execute(conn)?;
        if claimed == 0 {
            return Ok(false);
        }

        // Every active asset gets a row, a quiet week too; a retired one only for weeks it ran.
        // active_days: days with a trip. active_hours stays 0 until driver online time is logged.
        diesel::sql_query(format!(
            "WITH trips AS ({trips}) \
             INSERT INTO invest_weekly_stats \
                    (asset_id, week_start, trips, distance_km, gross_fares, investor_pool, active_days, active_hours) \
             SELECT a.id, $1::date, count(tr.asset_id)::int4, COALESCE(sum(tr.distance_km), 0)::numeric(10,2), \
                    COALESCE(sum(tr.fare_kobo), 0)::int8, \
                    floor(COALESCE(sum(tr.fare_kobo), 0) * p.investor_pct / 100)::int8, \
                    count(DISTINCT (tr.done_at AT TIME ZONE '{tz}')::date)::int4, 0 \
               FROM invest_assets a \
               JOIN invest_plans p ON p.id = a.plan_id \
               LEFT JOIN trips tr ON tr.asset_id = a.id \
              GROUP BY a.id, a.status, p.investor_pct \
             HAVING a.status = 'active' OR count(tr.asset_id) > 0 \
             ON CONFLICT (asset_id, week_start) DO NOTHING",
            trips = week_trips(), tz = LAGOS
        ))
        .bind::<Text, _>(week_start)
        .execute(conn)?;

        // Trip rows missed at completion, so the week's trip list is whole.
        diesel::sql_query(trip_earnings_insert(&week_trips()))
            .bind::<Text, _>(week_start)
            .execute(conn)?;

        diesel::sql_query(format!(
            "WITH trips AS ({trips}), \
             earned AS ( \
               INSERT INTO invest_earnings (slot_id, asset_id, owner_id, week_start, amount) \
               SELECT s.id, s.asset_id, o.owner_id, $1::date, floor(sum(tr.fare_kobo * s.slot_pct) / 100)::int8 \
                 FROM trips tr \
                 JOIN invest_slots s ON s.asset_id = tr.asset_id AND s.slot_pct IS NOT NULL \
                 JOIN invest_slot_owners o ON o.slot_id = s.id AND o.holder = 'investor' \
                  AND o.held_from <= tr.done_at AND (o.held_to IS NULL OR o.held_to > tr.done_at) \
                GROUP BY s.id, s.asset_id, o.owner_id \
               ON CONFLICT (slot_id, owner_id, week_start) DO NOTHING \
               RETURNING owner_id, amount) \
             INSERT INTO invest_notifications (user_id, kind, data) \
             SELECT owner_id, 'weekly_update', jsonb_build_object('week_start', $1::date, 'amount', sum(amount)) \
               FROM earned GROUP BY owner_id",
            trips = week_trips()
        ))
        .bind::<Text, _>(week_start)
        .execute(conn)?;
        Ok(true)
    })
}

async fn close_due_weeks(pool: &web::Data<DbPool>) {
    let due = match blocking(pool, |conn| Ok(due_weeks(conn)?)).await {
        Ok(due) => due,
        Err(e) => return eprintln!("Invest: could not list weeks to close: {}", e.message()),
    };
    for week in due {
        let week_start = week.week_start;
        let closing = week_start.clone();
        match blocking(pool, move |conn| Ok(close_week(conn, &closing)?)).await {
            Ok(true) => println!("Invest: week of {} closed", week_start),
            Ok(false) => {},
            Err(e) => {
                // Later weeks wait, so earnings are always written in order.
                eprintln!("Invest: week of {} not closed: {}", week_start, e.message());
                return;
            }
        }
    }
}

// ---------- Withdrawal PIN ----------
//
// A 4-digit PIN guards everything that moves an investor's money: withdrawals and changing the
// bank account they're paid to. Only an argon2 hash is stored (back_investor_pins); a PIN is
// checked against it, never read back. 5 wrong PINs lock it for 30 minutes. Forgotten PINs
// need support for now: there's no reset flow.

const PIN_MAX_ATTEMPTS: i32 = 5;
const PIN_LOCK_MINS: i32 = 30;

fn valid_pin(pin: &str) -> bool {
    pin.len() == 4 && pin.bytes().all(|b| b.is_ascii_digit())
}

fn hash_pin(pin: &str) -> Result<String, ApiError> {
    use argon2::password_hash::{ rand_core::OsRng, PasswordHasher, SaltString };
    let salt = SaltString::generate(&mut OsRng);
    argon2::Argon2::default()
        .hash_password(pin.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| ApiError::Internal(format!("Could not hash the PIN: {}", e)))
}

fn pin_matches(pin: &str, stored: &str) -> bool {
    use argon2::password_hash::{ PasswordHash, PasswordVerifier };
    PasswordHash::new(stored)
        .map(|hash| argon2::Argon2::default().verify_password(pin.as_bytes(), &hash).is_ok())
        .unwrap_or(false)
}

// Security notices the website shows (pin_locked, pin_changed, bank_details_changed), so an
// investor hears about it if it wasn't them. A notification needs the investor account, so it
// is created if this user has none yet.
fn notify_security(conn: &mut PgConnection, user: uuid::Uuid, kind: &str, data: serde_json::Value) -> QueryResult<()> {
    diesel::sql_query("INSERT INTO invest_accounts (user_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind::<SqlUuid, _>(user)
        .execute(conn)?;
    diesel::sql_query("INSERT INTO invest_notifications (user_id, kind, data) VALUES ($1, $2, $3)")
        .bind::<SqlUuid, _>(user)
        .bind::<Text, _>(kind)
        .bind::<diesel::sql_types::Jsonb, _>(data)
        .execute(conn)?;
    Ok(())
}

#[derive(QueryableByName)]
struct StoredPin {
    #[diesel(sql_type = Text)]
    pin_hash: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    failed_attempts: i32,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    locked: bool,
}

// Checks the investor's PIN. Commits on its own, so a wrong guess is counted even though the
// caller then stops; call it before (not inside) the transaction it guards.
fn verify_pin(conn: &mut PgConnection, user: uuid::Uuid, pin: Option<&str>) -> Result<(), ApiError> {
    use diesel::sql_types::Integer;

    let pin = pin.map(str::trim).filter(|p| !p.is_empty())
        .ok_or_else(|| ApiError::BadRequest("pin is required".into()))?;

    conn.transaction::<Result<(), ApiError>, ApiError, _>(|conn| {
        let stored: StoredPin = diesel::sql_query(
            "SELECT pin_hash, failed_attempts, COALESCE(locked_until > now(), false) AS locked \
               FROM back_investor_pins WHERE user_id = $1 FOR UPDATE"
        )
        .bind::<SqlUuid, _>(user)
        .get_result(conn)
        .optional()?
        .ok_or_else(|| ApiError::Forbidden("Set a withdrawal PIN first".into()))?;

        if stored.locked {
            return Ok(Err(ApiError::Forbidden(format!(
                "Too many wrong PINs; try again in up to {} minutes", PIN_LOCK_MINS
            ))));
        }
        if valid_pin(pin) && pin_matches(pin, &stored.pin_hash) {
            diesel::sql_query(
                "UPDATE back_investor_pins SET failed_attempts = 0, locked_until = NULL WHERE user_id = $1"
            ).bind::<SqlUuid, _>(user).execute(conn)?;
            return Ok(Ok(()));
        }

        let failed = stored.failed_attempts + 1;
        if failed >= PIN_MAX_ATTEMPTS {
            diesel::sql_query(
                "UPDATE back_investor_pins SET failed_attempts = 0, \
                        locked_until = now() + make_interval(mins => $2) WHERE user_id = $1"
            ).bind::<SqlUuid, _>(user).bind::<Integer, _>(PIN_LOCK_MINS).execute(conn)?;
            notify_security(conn, user, "pin_locked", json!({
                "attempts": PIN_MAX_ATTEMPTS,
                "locked_until": (chrono::Utc::now() + chrono::Duration::minutes(PIN_LOCK_MINS as i64)).to_rfc3339(),
            }))?;
            Ok(Err(ApiError::Forbidden(format!("Wrong PIN; locked for {} minutes", PIN_LOCK_MINS))))
        } else {
            diesel::sql_query("UPDATE back_investor_pins SET failed_attempts = $2 WHERE user_id = $1")
                .bind::<SqlUuid, _>(user).bind::<Integer, _>(failed).execute(conn)?;
            Ok(Err(ApiError::Forbidden(format!("Wrong PIN; {} tries left", PIN_MAX_ATTEMPTS - failed))))
        }
    })?
}

#[derive(Deserialize)]
pub struct CreatePin {
    pub pin: String,
}

// POST /invest/pin: the first PIN. 409 if one is set already (change it with PUT).
pub async fn create_pin(auth: AuthUser, pool: web::Data<DbPool>, body: web::Json<CreatePin>) -> HttpResponse {
    let user = auth.user_id;
    let pin = body.into_inner().pin;
    if !valid_pin(pin.trim()) {
        return ApiError::BadRequest("pin must be 4 digits".into()).response();
    }

    let result = blocking(&pool, move |conn| {
        let hash = hash_pin(pin.trim())?;
        let created = diesel::sql_query(
            "INSERT INTO back_investor_pins (user_id, pin_hash) VALUES ($1, $2) ON CONFLICT (user_id) DO NOTHING"
        )
        .bind::<SqlUuid, _>(user)
        .bind::<Text, _>(hash)
        .execute(conn)?;
        if created == 0 {
            return Err(ApiError::Conflict("A PIN is already set; change it instead".into()));
        }
        Ok(())
    }).await;

    match result {
        Ok(()) => HttpResponse::Created().json(json!({ "pin_set": true })),
        Err(e) => e.response(),
    }
}

#[derive(Deserialize)]
pub struct ChangePin {
    pub current_pin: String,
    pub new_pin: String,
}

// PUT /invest/pin: needs the current PIN.
pub async fn change_pin(auth: AuthUser, pool: web::Data<DbPool>, body: web::Json<ChangePin>) -> HttpResponse {
    let user = auth.user_id;
    let req = body.into_inner();
    if !valid_pin(req.new_pin.trim()) {
        return ApiError::BadRequest("new_pin must be 4 digits".into()).response();
    }

    let result = blocking(&pool, move |conn| {
        verify_pin(conn, user, Some(&req.current_pin))?;
        let hash = hash_pin(req.new_pin.trim())?;
        diesel::sql_query(
            "UPDATE back_investor_pins SET pin_hash = $2, failed_attempts = 0, locked_until = NULL, updated_at = now() \
              WHERE user_id = $1"
        )
        .bind::<SqlUuid, _>(user)
        .bind::<Text, _>(hash)
        .execute(conn)?;
        notify_security(conn, user, "pin_changed", json!({}))?;
        Ok(())
    }).await;

    match result {
        Ok(()) => HttpResponse::Ok().json(json!({ "pin_set": true })),
        Err(e) => e.response(),
    }
}

// ---------- Bank details ----------

#[derive(Deserialize)]
pub struct BankDetails {
    // 10-digit NUBAN.
    pub account_number: String,
    // Korapay bank code, from GET /payments/banks.
    pub bank_code: String,
    pub pin: Option<String>,
}

// PUT /invest/bank-details: where withdrawals are paid. Needs the PIN, so a stolen session
// can't redirect payouts. Creates the investor account if this is the user's first step.
pub async fn set_bank_details(auth: AuthUser, pool: web::Data<DbPool>, body: web::Json<BankDetails>) -> HttpResponse {
    let user = auth.user_id;
    let req = body.into_inner();
    let account_number = req.account_number.trim().to_string();
    let bank_code = req.bank_code.trim().to_string();
    if account_number.len() != 10 || !account_number.bytes().all(|b| b.is_ascii_digit()) {
        return ApiError::BadRequest("account_number must be 10 digits".into()).response();
    }
    if bank_code.is_empty() {
        return ApiError::BadRequest("bank_code is required".into()).response();
    }

    let result = blocking(&pool, move |conn| {
        verify_pin(conn, user, req.pin.as_deref())?;
        let details = json!({ "bank_account_number": account_number, "bank_code": bank_code });
        conn.transaction(|conn| {
            diesel::sql_query(
                "INSERT INTO invest_accounts (user_id, acct_details) VALUES ($1, $2) \
                 ON CONFLICT (user_id) DO UPDATE SET acct_details = EXCLUDED.acct_details"
            )
            .bind::<SqlUuid, _>(user)
            .bind::<diesel::sql_types::Jsonb, _>(&details)
            .execute(conn)?;
            // Only the last 4 digits go in the notice.
            notify_security(conn, user, "bank_details_changed", json!({
                "account_ending": &account_number[account_number.len() - 4..],
                "bank_code": bank_code,
            }))
        })?;
        Ok(details)
    }).await;

    match result {
        Ok(details) => HttpResponse::Ok().json(json!({ "acct_details": details })),
        Err(e) => e.response(),
    }
}

// ---------- Withdrawals ----------

#[derive(Deserialize, Default)]
pub struct WithdrawalRequest {
    // Kobo. Leave out for the whole balance.
    #[serde(default)]
    pub amount: Option<i64>,
    #[serde(default)]
    pub pin: Option<String>,
}

#[derive(QueryableByName)]
struct Balance {
    #[diesel(sql_type = BigInt)]
    balance: i64,
    #[diesel(sql_type = BigInt)]
    this_month: i64,
    #[diesel(sql_type = Nullable<Text>)]
    account_number: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    bank_code: Option<String>,
}

#[derive(QueryableByName)]
struct NewWithdrawal {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

// POST /invest/withdrawals. One a calendar month (Lagos), failed ones don't count; up to the
// balance. Paid straight away; the sweep picks up any payout that is still pending.
pub async fn request_withdrawal(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<WithdrawalRequest>,
) -> HttpResponse {
    let owner = auth.user_id;
    let WithdrawalRequest { amount: asked, pin } = body.into_inner();
    if matches!(asked, Some(amount) if amount <= 0) {
        return ApiError::BadRequest("amount must be more than 0".into()).response();
    }

    if let Err(e) = blocking(&pool, move |conn| verify_pin(conn, owner, pin.as_deref())).await {
        return e.response();
    }

    let result = blocking(&pool, move |conn| conn.transaction(|conn| {
        // Locks the account, so two requests can't both pass the checks below.
        let balance: Option<Balance> = diesel::sql_query(format!(
            "SELECT w.balance::int8 AS balance, \
                    (SELECT count(*) FROM invest_withdrawals x \
                      WHERE x.owner_id = a.user_id AND x.status <> 'failed' \
                        AND date_trunc('month', x.requested_at AT TIME ZONE '{tz}') \
                          = date_trunc('month', now() AT TIME ZONE '{tz}')) AS this_month, \
                    a.acct_details->>'bank_account_number' AS account_number, \
                    a.acct_details->>'bank_code' AS bank_code \
               FROM invest_accounts a \
               JOIN invest_wallet w ON w.user_id = a.user_id \
              WHERE a.user_id = $1 \
                FOR UPDATE OF a",
            tz = LAGOS
        ))
        .bind::<SqlUuid, _>(owner)
        .get_result(conn)
        .optional()?;

        let balance = balance.ok_or_else(|| ApiError::Forbidden("No investor account".into()))?;
        if balance.account_number.is_none() || balance.bank_code.is_none() {
            return Err(ApiError::BadRequest("Add your bank account before withdrawing".into()));
        }
        if balance.this_month > 0 {
            return Err(ApiError::Conflict("You have already withdrawn this month".into()));
        }
        let amount = asked.unwrap_or(balance.balance);
        if amount <= 0 {
            return Err(ApiError::BadRequest("Nothing to withdraw yet".into()));
        }
        if amount > balance.balance {
            return Err(ApiError::BadRequest(format!("amount is more than your balance of {} kobo", balance.balance)));
        }

        let created: NewWithdrawal = diesel::sql_query(
            "INSERT INTO invest_withdrawals (owner_id, amount, reference) \
             VALUES ($1, $2, 'iw_' || replace(gen_random_uuid()::text, '-', '')) RETURNING id"
        )
        .bind::<SqlUuid, _>(owner)
        .bind::<BigInt, _>(amount)
        .get_result(conn)?;
        Ok((created.id, amount))
    })).await;

    match result {
        Ok((id, amount)) => {
            let pool = pool.clone();
            tokio::spawn(async move { pay_withdrawal(&pool, id).await; });
            HttpResponse::Created().json(json!({ "id": id, "amount": amount, "status": "pending" }))
        }
        Err(e) => e.response(),
    }
}

#[derive(QueryableByName)]
struct PayoutTarget {
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = BigInt)]
    amount: i64,
    #[diesel(sql_type = Text)]
    reference: String,
    #[diesel(sql_type = Nullable<Text>)]
    account_number: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    bank_code: Option<String>,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Text)]
    email: String,
}

// Settles a withdrawal as paid or failed, once, and tells the investor. A failed one no longer
// counts against the balance or the month (invest_wallet), so they can ask again.
fn settle(conn: &mut PgConnection, id: i64, paid: bool, failure: Option<String>) -> QueryResult<()> {
    diesel::sql_query(
        "WITH done AS ( \
           UPDATE invest_withdrawals \
              SET status = CASE WHEN $2 THEN 'paid' ELSE 'failed' END, \
                  paid_at = CASE WHEN $2 THEN now() END, failure = $3 \
            WHERE id = $1 AND status IN ('pending', 'processing') \
            RETURNING id, owner_id, amount) \
         INSERT INTO invest_notifications (user_id, kind, data) \
         SELECT owner_id, CASE WHEN $2 THEN 'withdrawal_paid' ELSE 'withdrawal_failed' END, \
                jsonb_build_object('withdrawal_id', id, 'amount', amount) \
           FROM done"
    )
    .bind::<BigInt, _>(id)
    .bind::<diesel::sql_types::Bool, _>(paid)
    .bind::<Nullable<Text>, _>(failure)
    .execute(conn)?;
    Ok(())
}

// Korapay is asked first, so a payout already sent is never sent twice.
async fn pay_withdrawal(pool: &web::Data<DbPool>, id: i64) {
    let target = blocking(pool, move |conn| Ok(diesel::sql_query(
        "SELECT w.status, w.amount, w.reference, \
                a.acct_details->>'bank_account_number' AS account_number, \
                a.acct_details->>'bank_code' AS bank_code, \
                COALESCE(u.username, 'ASAP investor') AS name, COALESCE(au.email::text, '') AS email \
           FROM invest_withdrawals w \
           JOIN invest_accounts a ON a.user_id = w.owner_id \
           LEFT JOIN app_custom_users u ON u.id = w.owner_id \
           LEFT JOIN auth.users au ON au.id = w.owner_id \
          WHERE w.id = $1 AND w.reference IS NOT NULL"
    )
    .bind::<BigInt, _>(id)
    .get_result::<PayoutTarget>(conn)
    .optional()?)).await;

    let target = match target {
        Ok(Some(target)) if target.status == "pending" || target.status == "processing" => target,
        Ok(_) => return,
        Err(e) => return eprintln!("Invest: withdrawal {}: could not load it: {}", id, e.message()),
    };

    let outcome = match korapay::query_payout(&target.reference).await {
        Ok(TransferStatus::NotFound) => {
            let (Some(account_number), Some(bank_code)) = (target.account_number.as_deref(), target.bank_code.as_deref()) else {
                return finish(pool, id, false, Some("Bank account not set".into())).await;
            };
            let naira = target.amount as f64 / 100.0;
            match korapay::disburse(&target.reference, naira, bank_code, account_number, &target.name, &target.email, "ASAP Invest withdrawal").await {
                Ok(()) => TransferStatus::Processing,
                // Korapay refused it outright, so nothing left the account.
                Err(e) => return finish(pool, id, false, Some(e)).await,
            }
        }
        Ok(status) => status,
        Err(e) => return eprintln!("Invest: withdrawal {}: {}", id, e),
    };

    match outcome {
        TransferStatus::Success => finish(pool, id, true, None).await,
        TransferStatus::Failed => finish(pool, id, false, Some("Korapay reports the payout failed".into())).await,
        TransferStatus::Processing | TransferStatus::NotFound => {
            let marked = blocking(pool, move |conn| Ok(diesel::sql_query(
                "UPDATE invest_withdrawals SET status = 'processing' WHERE id = $1 AND status = 'pending'"
            ).bind::<BigInt, _>(id).execute(conn)?)).await;
            if let Err(e) = marked {
                eprintln!("Invest: withdrawal {}: could not mark it processing: {}", id, e.message());
            }
        }
    }
}

async fn finish(pool: &web::Data<DbPool>, id: i64, paid: bool, failure: Option<String>) {
    if let Some(reason) = &failure {
        eprintln!("Invest: withdrawal {} failed: {}", id, reason);
    }
    if let Err(e) = blocking(pool, move |conn| Ok(settle(conn, id, paid, failure)?)).await {
        eprintln!("Invest: withdrawal {}: could not record the outcome: {}", id, e.message());
    }
}

#[derive(QueryableByName)]
struct WithdrawalId {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

async fn sweep_withdrawals(pool: &web::Data<DbPool>) {
    let open = blocking(pool, |conn| Ok(diesel::sql_query(
        "SELECT id FROM invest_withdrawals WHERE status IN ('pending', 'processing') ORDER BY requested_at LIMIT 200"
    ).load::<WithdrawalId>(conn)?)).await;
    match open {
        Ok(open) => for w in open { pay_withdrawal(pool, w.id).await },
        Err(e) => eprintln!("Invest: could not load open withdrawals: {}", e.message()),
    }
}

// ---------- Slot purchases ----------
//
// Reserve, pay, settle. Reserving locks the asset row, so concurrent buyers (any worker, any
// instance) queue on it, then holds open slots in a back_invest_purchases row for the checkout
// window. One buyer may take every open slot. Settling (verify endpoint, sweep) asks Korapay
// and, on success, hands the slots over under the same lock.

// How long a checkout holds its slots.
const PURCHASE_HOLD_MINS: i32 = 30;
// An expired purchase is still checked this long for a payment that lands late.
const LATE_PURCHASE_WINDOW_MINS: i32 = 6 * 60;

// Slots of the asset that are open and held by no live checkout.
const FREE_SLOTS: &str =
    "SELECT s.id FROM invest_slots s \
      WHERE s.asset_id = $1 AND s.holder = 'open' \
        AND NOT EXISTS (SELECT 1 FROM back_invest_purchases p \
                         WHERE p.status = 'pending' AND p.expires_at > now() \
                           AND s.id = ANY(p.slot_ids))";

#[derive(Deserialize)]
pub struct PurchaseRequest {
    pub asset_id: uuid::Uuid,
    // How many slots; defaults to 1. Up to every open slot of the asset.
    #[serde(default)]
    pub slots: Option<i32>,
    // Where Korapay sends the buyer after the payment window.
    pub redirect_url: String,
}

#[derive(QueryableByName)]
struct ForSale {
    #[diesel(sql_type = Text)]
    code: String,
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    funding_open: bool,
    #[diesel(sql_type = BigInt)]
    slot_price: i64,
}

#[derive(QueryableByName)]
struct SlotId {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[derive(QueryableByName)]
struct Contact {
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Text)]
    email: String,
}

fn contact(conn: &mut PgConnection, user: uuid::Uuid) -> QueryResult<Contact> {
    diesel::sql_query(
        "SELECT COALESCE((SELECT username FROM app_custom_users WHERE id = $1), 'ASAP investor') AS name, \
                COALESCE((SELECT email::text FROM auth.users WHERE id = $1), '') AS email"
    )
    .bind::<SqlUuid, _>(user)
    .get_result(conn)
}

struct Reserved {
    reference: String,
    amount: i64,
    slots: usize,
    code: String,
    contact: Contact,
}

// POST /invest/purchases
pub async fn start_purchase(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<PurchaseRequest>,
) -> HttpResponse {
    use diesel::sql_types::{ Array, Integer };

    let buyer = auth.user_id;
    let req = body.into_inner();
    let wanted = req.slots.unwrap_or(1);
    if wanted <= 0 {
        return ApiError::BadRequest("slots must be at least 1".into()).response();
    }
    let redirect_url = req.redirect_url.trim().to_string();
    if redirect_url.is_empty() {
        return ApiError::BadRequest("redirect_url is required".into()).response();
    }
    let asset_id = req.asset_id;

    let reserved = blocking(&pool, move |conn| conn.transaction(|conn| {
        // The lock every buyer of this asset waits on.
        let asset: ForSale = diesel::sql_query(
            "SELECT a.code, a.status, a.funding_ends_at > now() AS funding_open, p.slot_price \
               FROM invest_assets a JOIN invest_plans p ON p.id = a.plan_id \
              WHERE a.id = $1 \
                FOR UPDATE OF a"
        )
        .bind::<SqlUuid, _>(asset_id)
        .get_result(conn)
        .optional()?
        .ok_or_else(|| ApiError::NotFound("Asset not found".into()))?;
        if asset.status != "funding" || !asset.funding_open {
            return Err(ApiError::Conflict(format!("{} is no longer selling slots", asset.code)));
        }

        let free: Vec<SlotId> = diesel::sql_query(format!("{} ORDER BY s.slot_no LIMIT $2", FREE_SLOTS))
            .bind::<SqlUuid, _>(asset_id)
            .bind::<Integer, _>(wanted)
            .load(conn)?;
        if free.len() < wanted as usize {
            return Err(ApiError::Conflict(format!(
                "Only {} slot(s) of {} are available right now", free.len(), asset.code
            )));
        }

        diesel::sql_query("INSERT INTO invest_accounts (user_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind::<SqlUuid, _>(buyer)
            .execute(conn)?;

        let slot_ids: Vec<i64> = free.iter().map(|s| s.id).collect();
        let amount = asset.slot_price * slot_ids.len() as i64;
        let reference = format!("ip_{}", uuid::Uuid::new_v4().simple());
        diesel::sql_query(
            "INSERT INTO back_invest_purchases \
                    (payment_reference, owner_id, asset_id, slot_ids, slot_price, amount, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(mins => $7))"
        )
        .bind::<Text, _>(&reference)
        .bind::<SqlUuid, _>(buyer)
        .bind::<SqlUuid, _>(asset_id)
        .bind::<Array<BigInt>, _>(&slot_ids)
        .bind::<BigInt, _>(asset.slot_price)
        .bind::<BigInt, _>(amount)
        .bind::<Integer, _>(PURCHASE_HOLD_MINS)
        .execute(conn)?;

        Ok(Reserved { reference, amount, slots: slot_ids.len(), code: asset.code, contact: contact(conn, buyer)? })
    })).await;

    let reserved = match reserved {
        Ok(reserved) => reserved,
        Err(e) => return e.response(),
    };

    let narration = format!("ASAP Invest {} slot(s) of {}", reserved.slots, reserved.code);
    let metadata = json!({ "kind": "invest_slots", "asset_id": asset_id, "slots": reserved.slots });
    let checkout = korapay::initialize_charge(
        &reserved.reference, reserved.amount / 100, &reserved.contact.email, &reserved.contact.name,
        &redirect_url, &narration, metadata,
    ).await;

    match checkout {
        Ok(checkout_url) => HttpResponse::Created().json(json!({
            "reference": reserved.reference,
            "checkout_url": checkout_url,
            "asset_code": reserved.code,
            "slots": reserved.slots,
            "amount": reserved.amount,
            "hold_minutes": PURCHASE_HOLD_MINS,
        })),
        Err(e) => {
            // Frees the slots for the next buyer.
            let reference = reserved.reference.clone();
            let failure = e.clone();
            let _ = blocking(&pool, move |conn| Ok(diesel::sql_query(
                "UPDATE back_invest_purchases SET status = 'failed', failure = $2 \
                  WHERE payment_reference = $1 AND status = 'pending'"
            ).bind::<Text, _>(reference).bind::<Text, _>(failure).execute(conn)?)).await;
            eprintln!("Invest: purchase {}: checkout not opened: {}", reserved.reference, e);
            ApiError::BadGateway("Could not open the payment page; try again".into()).response()
        }
    }
}

#[derive(QueryableByName)]
struct Purchase {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    payment_reference: String,
    #[diesel(sql_type = SqlUuid)]
    owner_id: uuid::Uuid,
    #[diesel(sql_type = BigInt)]
    amount: i64,
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    expired: bool,
}

const PURCHASE_COLUMNS: &str =
    "id, payment_reference, owner_id, amount, status, expires_at <= now() AS expired";

enum Handover {
    Done,
    // Paid, but the slots are gone; the money goes back.
    SlotsTaken,
    // Settled already by another call.
    Nothing,
}

// Gives the buyer the purchase's slots, under the asset lock. A purchase past its hold only
// gets them if no one else holds them now. The last open slot moves the asset to 'acquiring'.
fn hand_over(conn: &mut PgConnection, purchase_id: i64) -> QueryResult<Handover> {
    #[derive(QueryableByName)]
    struct Locked {
        #[diesel(sql_type = SqlUuid)]
        asset_id: uuid::Uuid,
        #[diesel(sql_type = Text)]
        status: String,
        #[diesel(sql_type = BigInt)]
        wanted: i64,
        #[diesel(sql_type = BigInt)]
        free: i64,
    }

    conn.transaction(|conn| {
        diesel::sql_query(
            "SELECT 1 FROM invest_assets WHERE id = (SELECT asset_id FROM back_invest_purchases WHERE id = $1) FOR UPDATE"
        ).bind::<BigInt, _>(purchase_id).execute(conn)?;

        // free: this purchase's slots still open and not held by another live checkout.
        let locked: Locked = diesel::sql_query(
            "SELECT p.asset_id, p.status, cardinality(p.slot_ids)::int8 AS wanted, \
                    (SELECT count(*) FROM invest_slots s \
                      WHERE s.id = ANY(p.slot_ids) AND s.holder = 'open' \
                        AND NOT EXISTS (SELECT 1 FROM back_invest_purchases o \
                                         WHERE o.id <> p.id AND o.status = 'pending' AND o.expires_at > now() \
                                           AND s.id = ANY(o.slot_ids))) AS free \
               FROM back_invest_purchases p WHERE p.id = $1 FOR UPDATE OF p"
        ).bind::<BigInt, _>(purchase_id).get_result(conn)?;

        if locked.status != "pending" && locked.status != "expired" {
            return Ok(Handover::Nothing);
        }
        if locked.free < locked.wanted {
            return Ok(Handover::SlotsTaken);
        }

        // slot_pct: the plan's investor_pct shared by its slots (14 / 4 = 3.5).
        diesel::sql_query(
            "WITH p AS (SELECT * FROM back_invest_purchases WHERE id = $1), \
             sold AS ( \
               UPDATE invest_slots s \
                  SET holder = 'investor', owner_id = p.owner_id, price_paid = p.slot_price, \
                      slot_pct = round(pl.investor_pct / pl.total_slots, 3), \
                      payment_ref = p.payment_reference || '-' || s.slot_no, acquired_at = now() \
                 FROM p, invest_assets a, invest_plans pl \
                WHERE s.id = ANY(p.slot_ids) AND a.id = s.asset_id AND pl.id = a.plan_id \
                RETURNING s.id, s.owner_id) \
             INSERT INTO invest_slot_owners (slot_id, holder, owner_id) \
             SELECT id, 'investor', owner_id FROM sold"
        ).bind::<BigInt, _>(purchase_id).execute(conn)?;

        diesel::sql_query(
            "UPDATE back_invest_purchases SET status = 'paid', paid_at = now(), failure = NULL WHERE id = $1"
        ).bind::<BigInt, _>(purchase_id).execute(conn)?;

        diesel::sql_query(
            "INSERT INTO invest_notifications (user_id, kind, data) \
             SELECT p.owner_id, 'slot_secured', \
                    jsonb_build_object('asset_id', p.asset_id, 'asset_code', a.code, \
                                       'slots', cardinality(p.slot_ids), 'amount', p.amount) \
               FROM back_invest_purchases p JOIN invest_assets a ON a.id = p.asset_id WHERE p.id = $1"
        ).bind::<BigInt, _>(purchase_id).execute(conn)?;

        // Fully funded: every investor holding a slot hears it once.
        let funded = diesel::sql_query(
            "UPDATE invest_assets SET status = 'acquiring' \
              WHERE id = $1 AND status = 'funding' \
                AND NOT EXISTS (SELECT 1 FROM invest_slots WHERE asset_id = $1 AND holder = 'open')"
        ).bind::<SqlUuid, _>(locked.asset_id).execute(conn)?;
        if funded > 0 {
            diesel::sql_query(
                "INSERT INTO invest_notifications (user_id, kind, data) \
                 SELECT DISTINCT s.owner_id, 'funding_complete', jsonb_build_object('asset_id', a.id, 'asset_code', a.code) \
                   FROM invest_slots s JOIN invest_assets a ON a.id = s.asset_id \
                  WHERE s.asset_id = $1 AND s.holder = 'investor'"
            ).bind::<SqlUuid, _>(locked.asset_id).execute(conn)?;
        }
        Ok(Handover::Done)
    })
}

fn mark_purchase(conn: &mut PgConnection, id: i64, status: &'static str, failure: Option<String>) -> QueryResult<()> {
    diesel::sql_query(
        "UPDATE back_invest_purchases SET status = $2, failure = COALESCE($3, failure) \
          WHERE id = $1 AND status IN ('pending', 'expired')"
    )
    .bind::<BigInt, _>(id)
    .bind::<Text, _>(status)
    .bind::<Nullable<Text>, _>(failure)
    .execute(conn)?;
    Ok(())
}

// Asks Korapay where the purchase's payment stands and settles it. Returns the status after.
async fn settle_purchase(pool: &web::Data<DbPool>, purchase: Purchase) -> Result<String, ApiError> {
    let id = purchase.id;
    if purchase.status != "pending" && purchase.status != "expired" {
        return Ok(purchase.status);
    }

    let charge = korapay::query_charge(&purchase.payment_reference).await.map_err(ApiError::BadGateway)?;
    match charge {
        korapay::ChargeStatus::Success { amount } if (amount * 100.0).round() as i64 >= purchase.amount => {
            match blocking(pool, move |conn| Ok(hand_over(conn, id)?)).await? {
                Handover::Done => Ok("paid".into()),
                Handover::Nothing => current_status(pool, id).await,
                Handover::SlotsTaken => {
                    let naira = purchase.amount / 100;
                    korapay::refund(&purchase.payment_reference, naira, "The slots were taken before your payment arrived")
                        .await
                        .map_err(ApiError::BadGateway)?;
                    blocking(pool, move |conn| Ok(mark_purchase(conn, id, "refunded", Some("Slots taken before payment arrived".into()))?)).await?;
                    Ok("refunded".into())
                }
            }
        }
        korapay::ChargeStatus::Success { amount } => {
            // Short payment: send it back rather than sell part of a slot.
            korapay::refund(&purchase.payment_reference, amount.floor() as i64, "Payment was less than the slot price")
                .await
                .map_err(ApiError::BadGateway)?;
            blocking(pool, move |conn| Ok(mark_purchase(conn, id, "refunded", Some(format!("Paid {} naira, short", amount)))?)).await?;
            Ok("refunded".into())
        }
        korapay::ChargeStatus::Failed => {
            blocking(pool, move |conn| Ok(mark_purchase(conn, id, "failed", Some("Korapay reports the charge failed".into()))?)).await?;
            Ok("failed".into())
        }
        korapay::ChargeStatus::Processing | korapay::ChargeStatus::NotFound => {
            if purchase.expired && purchase.status == "pending" {
                blocking(pool, move |conn| Ok(mark_purchase(conn, id, "expired", None)?)).await?;
                return Ok("expired".into());
            }
            Ok(purchase.status)
        }
    }
}

async fn current_status(pool: &web::Data<DbPool>, id: i64) -> Result<String, ApiError> {
    #[derive(QueryableByName)]
    struct Status {
        #[diesel(sql_type = Text)]
        status: String,
    }
    let row: Status = blocking(pool, move |conn| Ok(diesel::sql_query(
        "SELECT status FROM back_invest_purchases WHERE id = $1"
    ).bind::<BigInt, _>(id).get_result(conn)?)).await?;
    Ok(row.status)
}

// POST /invest/purchases/{reference}/verify: the app calls it when Korapay redirects back.
pub async fn verify_purchase(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let reference = path.into_inner();
    let me = auth.user_id;
    let found = blocking(&pool, move |conn| Ok(diesel::sql_query(format!(
        "SELECT {} FROM back_invest_purchases WHERE payment_reference = $1 AND owner_id = $2", PURCHASE_COLUMNS
    ))
    .bind::<Text, _>(reference)
    .bind::<SqlUuid, _>(me)
    .get_result::<Purchase>(conn)
    .optional()?)).await;

    let purchase = match found {
        Ok(Some(purchase)) => purchase,
        Ok(None) => return ApiError::NotFound("No purchase of yours under this reference".into()).response(),
        Err(e) => return e.response(),
    };
    let reference = purchase.payment_reference.clone();
    match settle_purchase(&pool, purchase).await {
        Ok(status) => HttpResponse::Ok().json(json!({ "reference": reference, "status": status })),
        Err(e) => e.response(),
    }
}

async fn sweep_purchases(pool: &web::Data<DbPool>) {
    let open = blocking(pool, |conn| Ok(diesel::sql_query(format!(
        "SELECT {} FROM back_invest_purchases \
          WHERE status = 'pending' \
             OR (status = 'expired' AND expires_at > now() - make_interval(mins => $1)) \
          ORDER BY created_at LIMIT 200", PURCHASE_COLUMNS
    ))
    .bind::<diesel::sql_types::Integer, _>(LATE_PURCHASE_WINDOW_MINS)
    .load::<Purchase>(conn)?)).await;

    let open = match open {
        Ok(open) => open,
        Err(e) => return eprintln!("Invest: could not load open purchases: {}", e.message()),
    };
    for purchase in open {
        let (reference, owner) = (purchase.payment_reference.clone(), purchase.owner_id);
        match settle_purchase(pool, purchase).await {
            Ok(status) if status == "paid" || status == "refunded" =>
                println!("Invest: purchase {} by {}: {}", reference, owner, status),
            Ok(_) => {},
            Err(e) => eprintln!("Invest: purchase {}: {}", reference, e.message()),
        }
    }
}

// ---------- Bridge sweep ----------
//
// Once an asset's funding window closes, the business buys every slot still open (the bridge
// loan) and the asset moves to 'acquiring'. Business slots earn nothing in invest_earnings (their
// share stays in the treasury) and can be sold on later through a bridge_sale transfer. An asset
// with a checkout still inside its hold waits for it; a payment landing after the sweep finds
// the slots gone and is refunded (settle_purchase).

#[derive(QueryableByName)]
struct AssetId {
    #[diesel(sql_type = SqlUuid)]
    id: uuid::Uuid,
}

enum Bridged {
    // The business took this many slots (0: investors had bought them all).
    Taken(usize),
    // A checkout still holds slots; next pass.
    Waiting,
    // Not due any more (another pass got there first).
    Nothing,
}

fn bridge_asset(conn: &mut PgConnection, asset_id: uuid::Uuid) -> QueryResult<Bridged> {
    conn.transaction(|conn| {
        // The same lock buyers take, so no purchase lands mid-sweep.
        let due = diesel::sql_query(
            "SELECT id FROM invest_assets \
              WHERE id = $1 AND status = 'funding' AND funding_ends_at <= now() FOR UPDATE"
        ).bind::<SqlUuid, _>(asset_id).load::<AssetId>(conn)?;
        if due.is_empty() {
            return Ok(Bridged::Nothing);
        }

        let held = diesel::sql_query(
            "SELECT asset_id AS id FROM back_invest_purchases \
              WHERE asset_id = $1 AND status = 'pending' AND expires_at > now() LIMIT 1"
        ).bind::<SqlUuid, _>(asset_id).load::<AssetId>(conn)?;
        if !held.is_empty() {
            return Ok(Bridged::Waiting);
        }

        let taken = diesel::sql_query(
            "WITH taken AS ( \
               UPDATE invest_slots s \
                  SET holder = 'business', owner_id = NULL, price_paid = pl.slot_price, \
                      slot_pct = round(pl.investor_pct / pl.total_slots, 3), acquired_at = now() \
                 FROM invest_assets a, invest_plans pl \
                WHERE s.asset_id = $1 AND s.holder = 'open' AND a.id = s.asset_id AND pl.id = a.plan_id \
                RETURNING s.id) \
             INSERT INTO invest_slot_owners (slot_id, holder, owner_id) \
             SELECT id, 'business', NULL FROM taken"
        ).bind::<SqlUuid, _>(asset_id).execute(conn)?;

        diesel::sql_query("UPDATE invest_assets SET status = 'acquiring' WHERE id = $1")
            .bind::<SqlUuid, _>(asset_id)
            .execute(conn)?;

        // Investors who already hold slots hear that funding closed, and how many the business took.
        diesel::sql_query(
            "INSERT INTO invest_notifications (user_id, kind, data) \
             SELECT DISTINCT s.owner_id, 'funding_complete', \
                    jsonb_build_object('asset_id', a.id, 'asset_code', a.code, 'bridged_slots', $2::int4) \
               FROM invest_slots s JOIN invest_assets a ON a.id = s.asset_id \
              WHERE s.asset_id = $1 AND s.holder = 'investor'"
        )
        .bind::<SqlUuid, _>(asset_id)
        .bind::<diesel::sql_types::Integer, _>(taken as i32)
        .execute(conn)?;
        Ok(Bridged::Taken(taken))
    })
}

async fn sweep_bridge(pool: &web::Data<DbPool>) {
    let due = blocking(pool, |conn| Ok(diesel::sql_query(
        "SELECT id FROM invest_assets WHERE status = 'funding' AND funding_ends_at <= now() \
          ORDER BY funding_ends_at LIMIT 100"
    ).load::<AssetId>(conn)?)).await;
    let due = match due {
        Ok(due) => due,
        Err(e) => return eprintln!("Invest: could not load assets to bridge: {}", e.message()),
    };
    for asset in due {
        let id = asset.id;
        match blocking(pool, move |conn| Ok(bridge_asset(conn, id)?)).await {
            Ok(Bridged::Taken(n)) => println!("Invest: asset {} funding closed, business took {} slot(s)", id, n),
            Ok(Bridged::Waiting) | Ok(Bridged::Nothing) => {},
            Err(e) => eprintln!("Invest: asset {}: bridge sweep failed: {}", id, e.message()),
        }
    }
}

const INVEST_SWEEP_EVERY_SECS: u64 = 300;

// Closes finished weeks and follows withdrawals up, for the life of the server.
pub fn spawn_invest_jobs(pool: web::Data<DbPool>) {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(INVEST_SWEEP_EVERY_SECS));
        loop {
            ticks.tick().await;
            // Purchases first, so a payment that landed in time is settled before its asset is bridged.
            sweep_purchases(&pool).await;
            sweep_bridge(&pool).await;
            close_due_weeks(&pool).await;
            sweep_withdrawals(&pool).await;
        }
    });
}

pub fn routes() -> Scope {
    web::scope("/invest")
        .route("/purchases", web::post().to(start_purchase))
        .route("/purchases/{reference}/verify", web::post().to(verify_purchase))
        .route("/withdrawals", web::post().to(request_withdrawal))
        .route("/pin", web::post().to(create_pin))
        .route("/pin", web::put().to(change_pin))
        .route("/bank-details", web::put().to(set_bank_details))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_are_four_digits() {
        assert!(valid_pin("0427"));
        assert!(!valid_pin("427"));
        assert!(!valid_pin("04271"));
        assert!(!valid_pin("04a7"));
    }

    #[test]
    fn a_pin_hash_only_matches_its_pin() {
        let stored = hash_pin("0427").ok().unwrap();
        assert!(!stored.contains("0427"));
        assert!(pin_matches("0427", &stored));
        assert!(!pin_matches("0428", &stored));
        assert!(!pin_matches("0427", "not a hash"));
        // Salted: the same PIN never hashes the same way twice.
        assert_ne!(stored, hash_pin("0427").ok().unwrap());
    }
}
