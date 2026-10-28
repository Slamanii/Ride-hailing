use actix_web::{ web, HttpRequest, FromRequest, dev::Payload };
use std::future::{ ready, Future, Ready };
use std::pin::Pin;
use std::env;
use diesel::prelude::*;
use serde::{ Deserialize, Serialize };
use uuid::Uuid;
use actix_web::http::StatusCode;
use crate::api::errors::{ blocking, json_extractor_error };
use crate::db::DbPool;

fn unauthorized() -> actix_web::Error {
    json_extractor_error(StatusCode::UNAUTHORIZED, "Unauthorized")
}

pub fn verify_secret(req: &HttpRequest) -> bool {
    let secret = env::var("ASAP_WEBHOOK_SECRET").unwrap_or_default();
    // An unset secret must not let an empty header through.
    if secret.is_empty() { return false; }
    req.headers()
        .get("x-asap-secret")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == secret)
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[allow(dead_code)]
    exp: usize,
}

fn bearer_claims(req: &HttpRequest) -> Option<Claims> {
    use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};

    let jwt_secret = env::var("SUPABASE_JWT_SECRET").unwrap_or_default();
    if jwt_secret.is_empty() { return None; }

    let token = req.headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if token.is_empty() { return None; }

    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_audience(&["authenticated"]);

    decode::<Claims>(token, &DecodingKey::from_secret(jwt_secret.as_bytes()), &validation)
        .ok()
        .map(|data| data.claims)
}

pub fn verify_bearer(req: &HttpRequest) -> bool {
    bearer_claims(req).is_some()
}

/// Requires a valid Supabase JWT (frontend endpoints).
pub struct AuthGuard;

/// Requires either a valid x-asap-secret OR a valid Supabase JWT (shared endpoints like ride-request).
pub struct AnyAuthGuard;

/// Who is calling: a signed-in user (Supabase JWT `sub`) or a backend service holding
/// x-asap-secret (Zazu). The secret wins when both are sent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Caller {
    User(Uuid),
    Service,
}

impl Caller {
    pub fn user_id(&self) -> Option<Uuid> {
        match self {
            Caller::User(id) => Some(*id),
            Caller::Service => None,
        }
    }
}

/// Requires a valid Supabase JWT and exposes the caller's auth.users id (the token's `sub`).
pub struct AuthUser {
    pub user_id: Uuid,
}

impl FromRequest for AuthGuard {
    type Error = actix_web::Error;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        if verify_bearer(req) {
            ready(Ok(AuthGuard))
        } else {
            ready(Err(unauthorized()))
        }
    }
}

impl FromRequest for AuthUser {
    type Error = actix_web::Error;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        match bearer_claims(req).and_then(|c| Uuid::parse_str(&c.sub).ok()) {
            Some(user_id) => ready(Ok(AuthUser { user_id })),
            None => ready(Err(unauthorized())),
        }
    }
}

impl FromRequest for AnyAuthGuard {
    type Error = actix_web::Error;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        if verify_secret(req) || verify_bearer(req) {
            ready(Ok(AnyAuthGuard))
        } else {
            ready(Err(unauthorized()))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AdminRole {
    // The one account that grants and revokes admins; changed only in the DB or in code.
    Superior,
    Admin,
}

/// Requires a valid Supabase JWT whose user is in back_admins (403 otherwise): the admin
/// endpoints and dashboard.
pub struct AdminUser {
    pub user_id: Uuid,
    pub role: AdminRole,
}

impl AdminUser {
    pub fn is_superior(&self) -> bool {
        self.role == AdminRole::Superior
    }
}

#[derive(QueryableByName)]
struct AdminRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    role: String,
}

impl FromRequest for AdminUser {
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let user = bearer_claims(req).and_then(|c| Uuid::parse_str(&c.sub).ok());
        let pool = req.app_data::<web::Data<DbPool>>().cloned();

        Box::pin(async move {
            let user_id = user.ok_or_else(unauthorized)?;
            let pool = pool.ok_or_else(|| json_extractor_error(StatusCode::INTERNAL_SERVER_ERROR, "No database"))?;
            let row = blocking(&pool, move |conn| Ok(diesel::sql_query("SELECT role FROM back_admins WHERE user_id = $1")
                .bind::<diesel::sql_types::Uuid, _>(user_id)
                .get_result::<AdminRow>(conn)
                .optional()?))
                .await
                .map_err(|e| json_extractor_error(StatusCode::INTERNAL_SERVER_ERROR, e.message()))?;
            let role = match row.as_ref().map(|r| r.role.as_str()) {
                Some("superior") => AdminRole::Superior,
                Some("admin") => AdminRole::Admin,
                _ => return Err(json_extractor_error(StatusCode::FORBIDDEN, "Admins only")),
            };
            Ok(AdminUser { user_id, role })
        })
    }
}

impl FromRequest for Caller {
    type Error = actix_web::Error;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        if verify_secret(req) {
            return ready(Ok(Caller::Service));
        }
        match bearer_claims(req).and_then(|c| Uuid::parse_str(&c.sub).ok()) {
            Some(user_id) => ready(Ok(Caller::User(user_id))),
            None => ready(Err(unauthorized())),
        }
    }
}
