use actix_web::{ HttpRequest, FromRequest, dev::Payload };
use std::future::{ ready, Ready };
use std::env;
use serde::Deserialize;
use uuid::Uuid;
use actix_web::http::StatusCode;
use crate::api::errors::json_extractor_error;

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
