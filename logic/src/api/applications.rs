use actix_web::{ web, HttpResponse };
use serde::{ Deserialize, Serialize };
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::sql_types::{ BigInt, Nullable, Text, Timestamptz, Uuid as SqlUuid };
use chrono::{ DateTime, Utc };
use uuid::Uuid;
use crate::api::errors::{ ApiError, blocking };
use crate::db::DbPool;
use crate::services::auth::{ AdminUser, AuthUser };
use crate::services::phone::normalize_phone;

// Applications to ride an ASAP-owned vehicle (an invest asset), in driver_applications
// (ASAP_APPS' table). The applicant submits here and reads their own row; an admin contacts
// them and decides in person. Approval (creating back_drivers and the asset_assignments row)
// comes with the admin endpoints.

#[derive(Deserialize)]
pub struct ApplicationForm {
    pub asset_type: String,
    pub full_name: String,
    pub phone: String,
    // Defaults to the signed-in account's email.
    #[serde(default)]
    pub email: Option<String>,
    pub area: String,
    pub nin: String,
    // Required for an EV.
    #[serde(default)]
    pub license_number: Option<String>,
    #[serde(default)]
    pub experience: Option<String>,
    #[serde(default)]
    pub guarantor_name: Option<String>,
    #[serde(default)]
    pub guarantor_phone: Option<String>,
}

fn optional(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn required(value: &str, field: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ApiError::BadRequest(format!("{} is required", field)));
    }
    Ok(value.to_string())
}

// The form, trimmed and checked; what the table's constraints would refuse is refused here
// with a message the applicant can act on.
struct Application {
    asset_type: String,
    full_name: String,
    phone: String,
    email: Option<String>,
    area: String,
    nin: String,
    license_number: Option<String>,
    experience: Option<String>,
    guarantor_name: Option<String>,
    guarantor_phone: Option<String>,
}

impl ApplicationForm {
    fn validate(self) -> Result<Application, ApiError> {
        let asset_type = self.asset_type.trim().to_string();
        if asset_type != "ev" && asset_type != "bicycle" {
            return Err(ApiError::BadRequest("asset_type must be 'ev' or 'bicycle'".into()));
        }
        let phone = normalize_phone(&self.phone)
            .ok_or_else(|| ApiError::BadRequest("phone is not a valid phone number".into()))?;
        let nin = self.nin.trim().to_string();
        if nin.len() != 11 || !nin.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ApiError::BadRequest("nin must be 11 digits".into()));
        }
        let license_number = optional(self.license_number);
        if asset_type == "ev" && license_number.is_none() {
            return Err(ApiError::BadRequest("license_number is required for an EV".into()));
        }
        let email = optional(self.email);
        if matches!(&email, Some(e) if !e.contains('@')) {
            return Err(ApiError::BadRequest("email is not valid".into()));
        }
        let guarantor_phone = match optional(self.guarantor_phone) {
            Some(raw) => Some(normalize_phone(&raw)
                .ok_or_else(|| ApiError::BadRequest("guarantor_phone is not a valid phone number".into()))?),
            None => None,
        };

        Ok(Application {
            full_name: required(&self.full_name, "full_name")?,
            area: required(&self.area, "area")?,
            asset_type, phone, email, nin, license_number, guarantor_phone,
            experience: optional(self.experience),
            guarantor_name: optional(self.guarantor_name),
        })
    }
}

// Matches the app's DriverApplication type.
#[derive(QueryableByName, Serialize)]
pub struct DriverApplication {
    #[diesel(sql_type = BigInt)]
    pub id: i64,
    #[diesel(sql_type = Text)]
    pub asset_type: String,
    #[diesel(sql_type = Text)]
    pub full_name: String,
    #[diesel(sql_type = Text)]
    pub phone: String,
    #[diesel(sql_type = Text)]
    pub area: String,
    #[diesel(sql_type = Text)]
    pub status: String,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub contacted_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub decided_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = Timestamptz)]
    pub created_at: DateTime<Utc>,
}

// POST /drivers/applications. One open (submitted or contacted) application per person.
pub async fn submit_application(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<ApplicationForm>,
) -> HttpResponse {
    let user = auth.user_id;
    let app = match body.into_inner().validate() {
        Ok(app) => app,
        Err(e) => return e.response(),
    };

    let result = blocking(&pool, move |conn| {
        use diesel::result::{ Error, DatabaseErrorKind };

        let inserted = diesel::sql_query(
            "INSERT INTO driver_applications \
                    (user_id, asset_type, full_name, phone, email, area, nin, license_number, \
                     experience, guarantor_name, guarantor_phone) \
             SELECT $1, $2, $3, $4, COALESCE($5, (SELECT email::text FROM auth.users WHERE id = $1)), \
                    $6, $7, $8, $9, $10, $11 \
             RETURNING id, asset_type, full_name, phone, area, status, contacted_at, decided_at, created_at"
        )
        .bind::<SqlUuid, _>(user)
        .bind::<Text, _>(app.asset_type)
        .bind::<Text, _>(app.full_name)
        .bind::<Text, _>(app.phone)
        .bind::<Nullable<Text>, _>(app.email)
        .bind::<Text, _>(app.area)
        .bind::<Text, _>(app.nin)
        .bind::<Nullable<Text>, _>(app.license_number)
        .bind::<Nullable<Text>, _>(app.experience)
        .bind::<Nullable<Text>, _>(app.guarantor_name)
        .bind::<Nullable<Text>, _>(app.guarantor_phone)
        .get_result::<DriverApplication>(conn);

        match inserted {
            Ok(application) => Ok(application),
            Err(Error::DatabaseError(DatabaseErrorKind::UniqueViolation, info))
                if info.constraint_name() == Some("driver_applications_one_open") =>
                Err(ApiError::Conflict("You already have an application in progress".into())),
            Err(Error::DatabaseError(DatabaseErrorKind::NotNullViolation, info))
                if info.column_name() == Some("email") =>
                Err(ApiError::BadRequest("email is required".into())),
            Err(e) => Err(e.into()),
        }
    }).await;

    match result {
        Ok(application) => HttpResponse::Created().json(application),
        Err(e) => e.response(),
    }
}

// ---------- Admin: review ----------
//
// The queue is worked by hand: an admin calls or emails the applicant (contact), then approves
// or rejects. Approving makes them a driver on an asset in one transaction: the back_drivers row
// (or, for an existing driver moving to an ASAP vehicle, its vehicle details), the
// asset_assignments row, and the asset going active, which tells its investors. Admins only;
// the deciding admin's id is kept as decided_by.

#[derive(Deserialize)]
pub struct ApplicationFilter {
    // Default: the open ones (submitted and contacted).
    #[serde(default)]
    pub status: Option<String>,
    // ev or bicycle
    #[serde(default)]
    pub asset_type: Option<String>,
}

// GET /admin/applications?status=&asset_type=
pub async fn list_applications(
    _admin: AdminUser,
    pool: web::Data<DbPool>,
    query: web::Query<ApplicationFilter>,
) -> HttpResponse {
    #[derive(QueryableByName)]
    struct Rows {
        #[diesel(sql_type = diesel::sql_types::Jsonb)]
        data: serde_json::Value,
    }
    let ApplicationFilter { status, asset_type } = query.into_inner();

    let result = blocking(&pool, move |conn| Ok(diesel::sql_query(
        "SELECT COALESCE(jsonb_agg(to_jsonb(x) ORDER BY x.created_at), '[]'::jsonb) AS data FROM ( \
           SELECT a.id, a.user_id, a.asset_type, a.full_name, a.phone, a.email, a.area, a.nin, \
                  a.license_number, a.experience, a.guarantor_name, a.guarantor_phone, a.status, \
                  a.admin_note, a.contacted_at, a.decided_at, a.decided_by, a.created_at, \
                  EXISTS (SELECT 1 FROM back_drivers d WHERE d.driver_id = a.user_id) AS already_driver \
             FROM driver_applications a \
            WHERE (CASE WHEN $1::text IS NULL THEN a.status IN ('submitted', 'contacted') ELSE a.status = $1 END) \
              AND ($2::text IS NULL OR a.asset_type = $2) \
            LIMIT 500) x"
    )
    .bind::<Nullable<Text>, _>(status)
    .bind::<Nullable<Text>, _>(asset_type)
    .get_result::<Rows>(conn)?
    .data)).await;

    match result {
        Ok(applications) => HttpResponse::Ok().json(serde_json::json!({ "applications": applications })),
        Err(e) => e.response(),
    }
}

#[derive(Deserialize, Default)]
pub struct ApplicationNote {
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(QueryableByName)]
struct ApplicationStatus {
    #[diesel(sql_type = Text)]
    status: String,
}

// Moves an open application to `to` (contacted, approved, rejected); a decision records when and
// who. 409 if it's already decided, 404 if there's none.
fn move_open(conn: &mut PgConnection, id: i64, to: &'static str, by: &str, note: Option<String>) -> Result<(), ApiError> {
    let updated = diesel::sql_query(
        "UPDATE driver_applications \
            SET status = $2, admin_note = COALESCE($3, admin_note), \
                contacted_at = CASE WHEN $2 = 'contacted' THEN COALESCE(contacted_at, now()) ELSE contacted_at END, \
                decided_at = CASE WHEN $2 = 'contacted' THEN decided_at ELSE now() END, \
                decided_by = CASE WHEN $2 = 'contacted' THEN decided_by ELSE $4 END \
          WHERE id = $1 AND status IN ('submitted', 'contacted')"
    )
    .bind::<BigInt, _>(id)
    .bind::<Text, _>(to)
    .bind::<Nullable<Text>, _>(note)
    .bind::<Text, _>(by)
    .execute(conn)?;
    if updated > 0 {
        return Ok(());
    }
    let current: Option<ApplicationStatus> = diesel::sql_query("SELECT status FROM driver_applications WHERE id = $1")
        .bind::<BigInt, _>(id)
        .get_result(conn)
        .optional()?;
    Err(match current {
        Some(a) => ApiError::Conflict(format!("Application {} is already {}", id, a.status)),
        None => ApiError::NotFound(format!("Application {} not found", id)),
    })
}

fn note_of(body: Option<web::Json<ApplicationNote>>) -> Option<String> {
    body.map(|b| b.into_inner()).unwrap_or_default().note
        .map(|n| n.trim().to_string()).filter(|n| !n.is_empty())
}

// POST /admin/applications/{id}/contact: the applicant was reached.
pub async fn contact_application(
    admin: AdminUser,
    pool: web::Data<DbPool>,
    path: web::Path<i64>,
    body: Option<web::Json<ApplicationNote>>,
) -> HttpResponse {
    let (id, by, note) = (path.into_inner(), admin.user_id.to_string(), note_of(body));
    let result = blocking(&pool, move |conn| move_open(conn, id, "contacted", &by, note)).await;
    match result {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({ "id": id, "status": "contacted" })),
        Err(e) => e.response(),
    }
}

// POST /admin/applications/{id}/reject
pub async fn reject_application(
    admin: AdminUser,
    pool: web::Data<DbPool>,
    path: web::Path<i64>,
    body: Option<web::Json<ApplicationNote>>,
) -> HttpResponse {
    let (id, by, note) = (path.into_inner(), admin.user_id.to_string(), note_of(body));
    let result = blocking(&pool, move |conn| move_open(conn, id, "rejected", &by, note)).await;
    match result {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({ "id": id, "status": "rejected" })),
        Err(e) => e.response(),
    }
}

#[derive(Deserialize)]
pub struct Approval {
    // The asset they'll ride: 'acquiring' or 'active', of the type they applied for, unassigned.
    pub asset_id: Uuid,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(QueryableByName)]
struct Applicant {
    #[diesel(sql_type = SqlUuid)]
    user_id: Uuid,
    #[diesel(sql_type = Text)]
    asset_type: String,
    #[diesel(sql_type = Text)]
    full_name: String,
    #[diesel(sql_type = Text)]
    phone: String,
    #[diesel(sql_type = Text)]
    email: String,
    #[diesel(sql_type = Nullable<Text>)]
    license_number: Option<String>,
    #[diesel(sql_type = Text)]
    status: String,
}

#[derive(QueryableByName)]
struct AssignableAsset {
    #[diesel(sql_type = Text)]
    code: String,
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = Text)]
    asset_type: String,
    #[diesel(sql_type = Nullable<Text>)]
    vehicle: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    assigned: bool,
}

// The app's vehicle_type for an asset type (DriverRequest::validate).
fn vehicle_type_for(asset_type: &str) -> &'static str {
    if asset_type == "ev" { "EV" } else { "Bike" }
}

// POST /admin/applications/{id}/approve { asset_id, note? }
pub async fn approve_application(
    admin: AdminUser,
    pool: web::Data<DbPool>,
    path: web::Path<i64>,
    body: web::Json<Approval>,
) -> HttpResponse {
    use diesel::result::{ Error, DatabaseErrorKind };

    let id = path.into_inner();
    let by = admin.user_id.to_string();
    let Approval { asset_id, note } = body.into_inner();
    let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());

    let result = blocking(&pool, move |conn| conn.transaction(|conn| {
        let app: Applicant = diesel::sql_query(
            "SELECT user_id, asset_type, full_name, phone, email, license_number, status \
               FROM driver_applications WHERE id = $1 FOR UPDATE"
        )
        .bind::<BigInt, _>(id)
        .get_result(conn)
        .optional()?
        .ok_or_else(|| ApiError::NotFound(format!("Application {} not found", id)))?;
        if app.status != "submitted" && app.status != "contacted" {
            return Err(ApiError::Conflict(format!("Application {} is already {}", id, app.status)));
        }

        // Locked, so two approvals can't hand out the same asset.
        let asset: AssignableAsset = diesel::sql_query(
            "SELECT a.code, a.status, p.asset_type, COALESCE(a.vehicle_name, p.name) AS vehicle, \
                    EXISTS (SELECT 1 FROM asset_assignments aa WHERE aa.asset_id = a.id AND aa.ended_at IS NULL) AS assigned \
               FROM invest_assets a JOIN invest_plans p ON p.id = a.plan_id \
              WHERE a.id = $1 FOR UPDATE OF a"
        )
        .bind::<SqlUuid, _>(asset_id)
        .get_result(conn)
        .optional()?
        .ok_or_else(|| ApiError::NotFound("Asset not found".into()))?;
        if asset.asset_type != app.asset_type {
            return Err(ApiError::Conflict(format!("{} is not a {} asset", asset.code, app.asset_type)));
        }
        if asset.status != "acquiring" && asset.status != "active" {
            return Err(ApiError::Conflict(format!("{} is {}; only acquired assets can be assigned", asset.code, asset.status)));
        }
        if asset.assigned {
            return Err(ApiError::Conflict(format!("{} already has a driver", asset.code)));
        }

        // A new driver starts offline; an existing one keeps their account and status, with the
        // asset's vehicle. Bank details come later through update-bank-details.
        diesel::sql_query(
            "INSERT INTO back_drivers (driver_id, driver_pubkey, name, email, phone, status, driver_location, \
                                       license_number, vehicle_type, driver_response, vehicle) \
             VALUES ($1, '{}'::jsonb, $2, $3, $4, 'offline', '{\"lat\": 0.0, \"lng\": 0.0, \"name\": \"Earth\"}'::jsonb, \
                     $5, $6, '\"Timeout\"'::jsonb, $7) \
             ON CONFLICT (driver_id) DO UPDATE SET \
                    vehicle_type = EXCLUDED.vehicle_type, vehicle = EXCLUDED.vehicle, \
                    license_number = COALESCE(EXCLUDED.license_number, back_drivers.license_number)"
        )
        .bind::<SqlUuid, _>(app.user_id)
        .bind::<Text, _>(&app.full_name)
        .bind::<Text, _>(&app.email)
        .bind::<Text, _>(&app.phone)
        .bind::<Nullable<Text>, _>(&app.license_number)
        .bind::<Text, _>(vehicle_type_for(&app.asset_type))
        .bind::<Nullable<Text>, _>(&asset.vehicle)
        .execute(conn)?;

        let assigned = diesel::sql_query("INSERT INTO asset_assignments (asset_id, driver_id) VALUES ($1, $2)")
            .bind::<SqlUuid, _>(asset_id)
            .bind::<SqlUuid, _>(app.user_id)
            .execute(conn);
        match assigned {
            Ok(_) => {},
            Err(Error::DatabaseError(DatabaseErrorKind::UniqueViolation, info))
                if info.constraint_name() == Some("asset_assignments_one_asset_per_driver") =>
                return Err(ApiError::Conflict("This driver already rides another asset; end that assignment first".into())),
            Err(e) => return Err(e.into()),
        }

        // First driver: the asset goes live and its investors hear about it.
        let activated = diesel::sql_query(
            "UPDATE invest_assets SET status = 'active', activated_at = COALESCE(activated_at, now()) \
              WHERE id = $1 AND status = 'acquiring'"
        )
        .bind::<SqlUuid, _>(asset_id)
        .execute(conn)?;
        if activated > 0 {
            diesel::sql_query(
                "INSERT INTO invest_notifications (user_id, kind, data) \
                 SELECT DISTINCT s.owner_id, 'asset_active', jsonb_build_object('asset_id', a.id, 'asset_code', a.code) \
                   FROM invest_slots s JOIN invest_assets a ON a.id = s.asset_id \
                  WHERE s.asset_id = $1 AND s.holder = 'investor'"
            )
            .bind::<SqlUuid, _>(asset_id)
            .execute(conn)?;
        }

        move_open(conn, id, "approved", &by, note)?;
        Ok((app.user_id, asset.code))
    })).await;

    match result {
        Ok((driver_id, asset_code)) => HttpResponse::Ok().json(serde_json::json!({
            "id": id,
            "status": "approved",
            "driver_id": driver_id,
            "asset_id": asset_id,
            "asset_code": asset_code,
        })),
        Err(e) => e.response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(asset_type: &str, license: Option<&str>) -> ApplicationForm {
        ApplicationForm {
            asset_type: asset_type.into(), full_name: " Ada Obi ".into(), phone: "0803 123 4567".into(),
            email: None, area: "Wuse".into(), nin: "12345678901".into(),
            license_number: license.map(Into::into), experience: None,
            guarantor_name: None, guarantor_phone: Some(" ".into()),
        }
    }

    #[test]
    fn an_ev_application_needs_a_licence() {
        assert!(form("ev", None).validate().is_err());
        assert!(form("ev", Some("ABC123")).validate().is_ok());
        assert!(form("bicycle", None).validate().is_ok());
    }

    #[test]
    fn the_form_is_cleaned_up() {
        let app = form("bicycle", None).validate().ok().unwrap();
        assert_eq!(app.full_name, "Ada Obi");
        assert!(app.phone.starts_with("+234"));
        assert_eq!(app.guarantor_phone, None);

        let mut bad_nin = form("bicycle", None);
        bad_nin.nin = "1234".into();
        assert!(bad_nin.validate().is_err());
    }
}
