use actix_web::{ web, Scope, HttpResponse, http::StatusCode };
use serde::{ Deserialize, Serialize };
use diesel::prelude::*;
use serde_json::{ json, Value };
use crate::db::{ DbPool };
use uuid::Uuid;
use crate::api::drivers::Driver;
use crate::api::errors::{ ApiError, blocking, json_error, message_body };
use crate::services::auth::{ AdminUser, AuthUser };
use crate::services::codes;

// Every handler here that takes AdminUser is admins only (back_admins). create-riders and
// create-drivers are not: they're the apps' signup calls, for any signed-in user.

pub async fn admin_dashboard(_admin: AdminUser, pool: web::Data<DbPool>) -> HttpResponse {
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
        // The app generates the plate; on this it draws a new one and resends.
        Error::DatabaseError(DatabaseErrorKind::UniqueViolation, info)
            if info.constraint_name() == Some("back_drivers_plate_number_key") =>
            ApiError::Conflict("plate_number is already registered; generate another".into()),
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

pub async fn get_riders(_admin: AdminUser, pool: web::Data<DbPool>) -> HttpResponse {
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


pub async fn get_drivers(_admin: AdminUser, pool: web::Data<DbPool>) -> HttpResponse {
    use crate::schema::back_drivers::dsl::back_drivers as drivers;

    let results = blocking(&pool, |conn| {
        Ok(drivers.limit(20).select(Driver::as_select()).load::<Driver>(conn)?)
    }).await;

    match results {
        Ok(data) => HttpResponse::Ok().json(data),
        Err(e) => e.response(),
    }
}

// ---------- Investment slot transfers ----------
// An approval only moves ownership: payment for the slot is settled by hand before an admin
// approves. invest_apply_transfer (ASAP_APPS' invest.sql) moves the slot, closes the old
// ownership record, opens the new one and notifies both sides, in one call. The deciding
// admin's id is kept as decided_by.

#[derive(Deserialize, Default)]
pub struct TransferDecision {
    #[serde(default)]
    pub note: Option<String>,
}

// The function raises 'Transfer N not found' / 'already approved' / 'not held by ...'.
fn transfer_db_error(e: diesel::result::Error) -> ApiError {
    use diesel::result::{ Error, DatabaseErrorKind };
    match e {
        Error::DatabaseError(_, info) if info.message().contains("not found") =>
            ApiError::NotFound(info.message().to_string()),
        Error::DatabaseError(DatabaseErrorKind::Unknown, info) =>
            ApiError::Conflict(info.message().to_string()),
        other => other.into(),
    }
}

pub async fn approve_transfer(
    admin: AdminUser,
    pool: web::Data<DbPool>,
    path: web::Path<i64>,
) -> HttpResponse {
    let transfer_id = path.into_inner();
    let decided_by = admin.user_id.to_string();

    let result = blocking(&pool, move |conn| {
        diesel::sql_query("SELECT invest_apply_transfer($1, $2)")
            .bind::<diesel::sql_types::BigInt, _>(transfer_id)
            .bind::<diesel::sql_types::Text, _>(decided_by)
            .execute(conn)
            .map_err(transfer_db_error)
    }).await;

    match result {
        Ok(_) => HttpResponse::Ok().json(json!({ "transfer_id": transfer_id, "status": "approved" })),
        Err(e) => e.response(),
    }
}

#[derive(QueryableByName)]
struct RejectedTransfer {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    slot_id: i64,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    to_owner: Uuid,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    from_owner: Option<Uuid>,
}

#[derive(QueryableByName)]
struct TransferStatus {
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
}

// Nothing moves; the request is closed and both sides are told.
pub async fn reject_transfer(
    admin: AdminUser,
    pool: web::Data<DbPool>,
    path: web::Path<i64>,
    body: Option<web::Json<TransferDecision>>,
) -> HttpResponse {
    use diesel::sql_types::{ BigInt, Nullable, Text, Uuid as SqlUuid };

    let transfer_id = path.into_inner();
    let decided_by = admin.user_id.to_string();
    let note = body.map(|b| b.into_inner()).unwrap_or_default().note;

    let result = blocking(&pool, move |conn| conn.transaction(|conn| {
        let rejected: Option<RejectedTransfer> = diesel::sql_query(
            "UPDATE invest_slot_transfers \
                SET status = 'rejected', decided_by = $2, decided_at = now(), note = COALESCE($3, note) \
              WHERE id = $1 AND status = 'requested' \
              RETURNING slot_id, to_owner, from_owner"
        )
        .bind::<BigInt, _>(transfer_id)
        .bind::<Text, _>(decided_by)
        .bind::<Nullable<Text>, _>(note)
        .get_result(conn)
        .optional()?;

        let Some(rejected) = rejected else {
            let current: Option<TransferStatus> = diesel::sql_query("SELECT status FROM invest_slot_transfers WHERE id = $1")
                .bind::<BigInt, _>(transfer_id)
                .get_result(conn)
                .optional()?;
            return Err(match current {
                Some(t) => ApiError::Conflict(format!("Transfer {} is already {}", transfer_id, t.status)),
                None => ApiError::NotFound(format!("Transfer {} not found", transfer_id)),
            });
        };

        diesel::sql_query(
            "INSERT INTO invest_notifications (user_id, kind, data) \
             SELECT u, 'transfer_rejected', jsonb_build_object('transfer_id', $1::int8, 'slot_id', $2::int8) \
               FROM unnest(ARRAY[$3, $4]::uuid[]) u WHERE u IS NOT NULL"
        )
        .bind::<BigInt, _>(transfer_id)
        .bind::<BigInt, _>(rejected.slot_id)
        .bind::<SqlUuid, _>(rejected.to_owner)
        .bind::<Nullable<SqlUuid>, _>(rejected.from_owner)
        .execute(conn)?;
        Ok(())
    })).await;

    match result {
        Ok(()) => HttpResponse::Ok().json(json!({ "transfer_id": transfer_id, "status": "rejected" })),
        Err(e) => e.response(),
    }
}

// ---------- Investment plans ----------
// A plan is an asset type sold in equal slots: asset_price (the whole vehicle, kobo) is set
// here and slot_price = asset_price / total_slots, so it has to divide into whole naira. Needs
// invest_plans.asset_price (ASAP_APPS).

#[derive(Deserialize)]
pub struct PlanRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    // 'ev' or 'bicycle'
    pub asset_type: String,
    pub total_slots: i32,
    // Kobo, the whole asset.
    pub asset_price: i64,
    // Defaults 14 (% of each fare to the asset's slots) and 168 (days, 24 weeks, before the
    // bridge sweep).
    #[serde(default)]
    pub investor_pct: Option<f64>,
    #[serde(default)]
    pub funding_days: Option<i32>,
    #[serde(default)]
    pub sort: Option<i32>,
}

impl PlanRequest {
    fn slot_price(&self) -> Result<i64, ApiError> {
        if self.name.trim().is_empty() {
            return Err(ApiError::BadRequest("name is required".into()));
        }
        if self.asset_type != "ev" && self.asset_type != "bicycle" {
            return Err(ApiError::BadRequest("asset_type must be 'ev' or 'bicycle'".into()));
        }
        if self.total_slots <= 0 {
            return Err(ApiError::BadRequest("total_slots must be more than 0".into()));
        }
        if self.asset_price <= 0 {
            return Err(ApiError::BadRequest("asset_price must be more than 0".into()));
        }
        if matches!(self.investor_pct, Some(pct) if !(pct > 0.0 && pct < 100.0)) {
            return Err(ApiError::BadRequest("investor_pct must be between 0 and 100".into()));
        }
        if matches!(self.funding_days, Some(days) if days <= 0) {
            return Err(ApiError::BadRequest("funding_days must be more than 0".into()));
        }
        // Slots are charged through Korapay in whole naira.
        let slots = self.total_slots as i64;
        if self.asset_price % (slots * 100) != 0 {
            return Err(ApiError::BadRequest(format!(
                "asset_price must split into {} slots of whole naira", self.total_slots
            )));
        }
        Ok(self.asset_price / slots)
    }
}

#[derive(QueryableByName, Serialize)]
struct CreatedPlan {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    description: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Text)]
    asset_type: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    total_slots: i32,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    asset_price: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    slot_price: i64,
    #[diesel(sql_type = diesel::sql_types::Double)]
    investor_pct: f64,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    funding_days: i32,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    active: bool,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    sort: i32,
}

// How long an asset sells slots before the business takes the rest: 24 weeks.
const DEFAULT_FUNDING_DAYS: i32 = 24 * 7;

pub async fn create_plan(
    _admin: AdminUser,
    pool: web::Data<DbPool>,
    body: web::Json<PlanRequest>,
) -> HttpResponse {
    use diesel::sql_types::{ BigInt, Double, Integer, Nullable, Text };

    let req = body.into_inner();
    let slot_price = match req.slot_price() {
        Ok(price) => price,
        Err(e) => return e.response(),
    };
    let description = req.description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());

    let result = blocking(&pool, move |conn| Ok(diesel::sql_query(
        "INSERT INTO invest_plans (name, description, asset_type, total_slots, slot_price, asset_price, \
                                   investor_pct, funding_days, sort) \
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7::numeric(5,2), 14), COALESCE($8, $10), COALESCE($9, 0)) \
         RETURNING id, name, description, asset_type, total_slots, asset_price, slot_price, \
                   investor_pct::float8 AS investor_pct, funding_days, active, sort"
    )
    .bind::<Text, _>(req.name.trim())
    .bind::<Nullable<Text>, _>(description)
    .bind::<Text, _>(req.asset_type)
    .bind::<Integer, _>(req.total_slots)
    .bind::<BigInt, _>(slot_price)
    .bind::<BigInt, _>(req.asset_price)
    .bind::<Nullable<Double>, _>(req.investor_pct)
    .bind::<Nullable<Integer>, _>(req.funding_days)
    .bind::<Nullable<Integer>, _>(req.sort)
    .bind::<Integer, _>(DEFAULT_FUNDING_DAYS)
    .get_result::<CreatedPlan>(conn)?)).await;

    match result {
        Ok(plan) => HttpResponse::Created().json(json!({ "plan": plan })),
        Err(e) => e.response(),
    }
}

// ---------- Investment assets ----------
// An asset is created 'funding' with one open slot per plan slot; investors fill them by
// buying, and after funding_days the business takes what's left (the bridge sweep). slot_pct
// and price_paid are written at purchase, from the plan, so later plan changes never touch a
// holder.

#[derive(Deserialize)]
pub struct AssetRequest {
    pub plan_id: i64,
    #[serde(default)]
    pub vehicle_name: Option<String>,
}

#[derive(QueryableByName)]
struct AssetPlan {
    #[diesel(sql_type = diesel::sql_types::Text)]
    asset_type: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    total_slots: i32,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    active: bool,
}

#[derive(QueryableByName, Serialize)]
struct CreatedAsset {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Text)]
    code: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    vehicle_name: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    funding_ends_at: chrono::DateTime<chrono::Utc>,
}

// What investors see in place of the asset type: EV-7K3Q, BK-M4XP.
fn asset_code_prefix(asset_type: &str) -> &'static str {
    match asset_type {
        "ev" => "EV",
        _ => "BK",
    }
}

const ASSET_CODE_ATTEMPTS: usize = 5;

pub async fn create_asset(
    _admin: AdminUser,
    pool: web::Data<DbPool>,
    body: web::Json<AssetRequest>,
) -> HttpResponse {
    use diesel::sql_types::{ BigInt, Integer, Nullable, Text, Uuid as SqlUuid };
    use diesel::result::{ Error, DatabaseErrorKind };

    let req = body.into_inner();
    let vehicle_name = req.vehicle_name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());

    let result = blocking(&pool, move |conn| {
        let plan: AssetPlan = diesel::sql_query("SELECT asset_type, total_slots, active FROM invest_plans WHERE id = $1")
            .bind::<BigInt, _>(req.plan_id)
            .get_result(conn)
            .optional()?
            .ok_or_else(|| ApiError::NotFound(format!("Plan {} not found", req.plan_id)))?;
        if !plan.active {
            return Err(ApiError::Conflict(format!("Plan {} is not active", req.plan_id)));
        }

        // A clashing code rolls its attempt back; the next one draws a new code.
        for _ in 0..ASSET_CODE_ATTEMPTS {
            let code = codes::asset_code(asset_code_prefix(&plan.asset_type));
            let vehicle_name = vehicle_name.clone();
            let attempt = conn.transaction(|conn| {
                let asset: CreatedAsset = diesel::sql_query(
                    "INSERT INTO invest_assets (code, plan_id, vehicle_name, funding_ends_at) \
                     SELECT $1, p.id, $3, now() + make_interval(days => p.funding_days) \
                       FROM invest_plans p WHERE p.id = $2 \
                     RETURNING id, code, status, vehicle_name, funding_ends_at"
                )
                .bind::<Text, _>(code)
                .bind::<BigInt, _>(req.plan_id)
                .bind::<Nullable<Text>, _>(vehicle_name)
                .get_result(conn)?;

                diesel::sql_query(
                    "INSERT INTO invest_slots (asset_id, slot_no) SELECT $1, generate_series(1, $2)"
                )
                .bind::<SqlUuid, _>(asset.id)
                .bind::<Integer, _>(plan.total_slots)
                .execute(conn)?;
                Ok::<_, Error>(asset)
            });
            match attempt {
                Ok(asset) => return Ok((asset, plan.total_slots)),
                Err(Error::DatabaseError(DatabaseErrorKind::UniqueViolation, info))
                    if info.constraint_name() == Some("invest_assets_code_key") => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(ApiError::Internal("Could not draw a free asset code; try again".into()))
    }).await;

    match result {
        Ok((asset, total_slots)) => HttpResponse::Created().json(json!({
            "asset": asset,
            "total_slots": total_slots,
            "open_slots": total_slots,
        })),
        Err(e) => e.response(),
    }
}

// ---------- Admins ----------
// The superior grants and revokes admins by email. The superior role itself never moves
// through here: changing it is a DB update or a code change.

// GET /admin/me: who the dashboard is signed in as (403 for non-admins, so the website can
// use it as its gate).
pub async fn me(admin: AdminUser) -> HttpResponse {
    HttpResponse::Ok().json(json!({ "user_id": admin.user_id, "role": admin.role }))
}

#[derive(QueryableByName)]
struct AdminList {
    #[diesel(sql_type = diesel::sql_types::Jsonb)]
    data: Value,
}

// GET /admin/admins: any admin may see who else is one.
pub async fn list_admins(_admin: AdminUser, pool: web::Data<DbPool>) -> HttpResponse {
    let result = blocking(&pool, |conn| Ok(diesel::sql_query(
        "SELECT COALESCE(jsonb_agg(jsonb_build_object( \
                  'user_id', a.user_id, 'email', u.email, 'role', a.role, \
                  'granted_by', a.granted_by, 'granted_at', a.granted_at) \
                ORDER BY a.role DESC, a.granted_at), '[]'::jsonb) AS data \
           FROM back_admins a JOIN auth.users u ON u.id = a.user_id"
    ).get_result::<AdminList>(conn)?.data)).await;
    match result {
        Ok(admins) => HttpResponse::Ok().json(json!({ "admins": admins })),
        Err(e) => e.response(),
    }
}

#[derive(Deserialize)]
pub struct GrantAdmin {
    // The account's sign-in email; it must already exist.
    pub email: String,
}

#[derive(QueryableByName)]
struct Granted {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    user_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    role: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    inserted: bool,
}

// POST /admin/admins { email }: superior only. Always grants 'admin', never 'superior'.
pub async fn grant_admin(admin: AdminUser, pool: web::Data<DbPool>, body: web::Json<GrantAdmin>) -> HttpResponse {
    if !admin.is_superior() {
        return ApiError::Forbidden("Only the superior admin can grant admin access".into()).response();
    }
    let email = body.into_inner().email.trim().to_lowercase();
    if email.is_empty() {
        return ApiError::BadRequest("email is required".into()).response();
    }
    let granted_by = admin.user_id;

    let result = blocking(&pool, move |conn| {
        let granted: Option<Granted> = diesel::sql_query(
            "WITH u AS (SELECT id FROM auth.users WHERE lower(email) = $1 LIMIT 1), \
             ins AS (INSERT INTO back_admins (user_id, role, granted_by) \
                     SELECT id, 'admin', $2 FROM u ON CONFLICT (user_id) DO NOTHING RETURNING user_id) \
             SELECT u.id AS user_id, a.role, EXISTS (SELECT 1 FROM ins) AS inserted \
               FROM u LEFT JOIN back_admins a ON a.user_id = u.id"
        )
        .bind::<diesel::sql_types::Text, _>(&email)
        .bind::<diesel::sql_types::Uuid, _>(granted_by)
        .get_result(conn)
        .optional()?;
        match granted {
            None => Err(ApiError::NotFound(format!("No account signs in with {}", email))),
            Some(g) if g.inserted => Ok(g.user_id),
            Some(g) => Err(ApiError::Conflict(format!(
                "{} is already {}", email, g.role.as_deref().unwrap_or("an admin")
            ))),
        }
    }).await;

    match result {
        Ok(user_id) => HttpResponse::Created().json(json!({ "user_id": user_id, "role": "admin" })),
        Err(e) => e.response(),
    }
}

// DELETE /admin/admins/{user_id}: superior only. Removes an admin; the superior can't be removed.
pub async fn revoke_admin(admin: AdminUser, pool: web::Data<DbPool>, path: web::Path<Uuid>) -> HttpResponse {
    if !admin.is_superior() {
        return ApiError::Forbidden("Only the superior admin can revoke admin access".into()).response();
    }
    let user_id = path.into_inner();
    let result = blocking(&pool, move |conn| Ok(diesel::sql_query(
        "DELETE FROM back_admins WHERE user_id = $1 AND role = 'admin'"
    ).bind::<diesel::sql_types::Uuid, _>(user_id).execute(conn)?)).await;

    match result {
        Ok(0) => ApiError::NotFound("No admin with that user_id (the superior can't be revoked here)".into()).response(),
        Ok(_) => HttpResponse::Ok().json(json!({ "user_id": user_id, "revoked": true })),
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
        .route("/me", web::get().to(me))
        .route("/admins", web::get().to(list_admins))
        .route("/admins", web::post().to(grant_admin))
        .route("/admins/{user_id}", web::delete().to(revoke_admin))
        .route("/dashboard/live", web::get().to(crate::api::dashboard::live))
        .route("/dashboard/overview", web::get().to(crate::api::dashboard::overview))
        .route("/dashboard/timeseries", web::get().to(crate::api::dashboard::timeseries))
        .route("/dashboard/drivers", web::get().to(crate::api::dashboard::drivers))
        .route("/dashboard/assets", web::get().to(crate::api::dashboard::assets))
        .route("/applications", web::get().to(crate::api::applications::list_applications))
        .route("/applications/{id}/contact", web::post().to(crate::api::applications::contact_application))
        .route("/applications/{id}/approve", web::post().to(crate::api::applications::approve_application))
        .route("/applications/{id}/reject", web::post().to(crate::api::applications::reject_application))
        .route("/disputes", web::get().to(crate::api::disputes::list_disputes))
        .route("/disputes/{id}/review", web::post().to(crate::api::disputes::review_dispute))
        .route("/invest/plans", web::post().to(create_plan))
        .route("/invest/assets", web::post().to(create_asset))
        .route("/invest/transfers/{id}/approve", web::post().to(approve_transfer))
        .route("/invest/transfers/{id}/reject", web::post().to(reject_transfer))
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
    // The ASAP vehicle id (ASP-XX-XX), generated by the app.
    #[serde(default)]
    pub plate_number: Option<String>,
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
    pub plate_number: Option<String>,
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
            plate_number: req.plate_number.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(asset_type: &str, total_slots: i32, asset_price: i64) -> PlanRequest {
        PlanRequest {
            name: "EV".into(), description: None, asset_type: asset_type.into(), total_slots, asset_price,
            investor_pct: None, funding_days: None, sort: None,
        }
    }

    #[test]
    fn slot_price_splits_the_asset_price_evenly() {
        assert_eq!(plan("ev", 4, 2_000_000_00).slot_price().ok(), Some(500_000_00));
        assert_eq!(plan("bicycle", 1, 450_000_00).slot_price().ok(), Some(450_000_00));
        assert!(plan("ev", 3, 100).slot_price().is_err());
        assert_eq!(plan("ev", 4, 400).slot_price().ok(), Some(100));
        assert!(plan("ev", 4, 200).slot_price().is_err()); // 50 kobo slots
        assert!(plan("car", 4, 400).slot_price().is_err());
        assert!(plan("ev", 0, 400).slot_price().is_err());
    }
}
