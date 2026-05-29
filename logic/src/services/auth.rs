use actix_web::{ HttpRequest, FromRequest, dev::Payload };
use std::future::{ ready, Ready };
use std::env;

pub fn verify_secret(req: &HttpRequest) -> bool {
    let secret = env::var("ASAP_WEBHOOK_SECRET").unwrap_or_default();
    req.headers()
        .get("x-asap-secret")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == secret)
        .unwrap_or(false)
}

pub fn verify_bearer(req: &HttpRequest) -> bool {
    use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Claims { sub: String, exp: usize }

    let jwt_secret = env::var("SUPABASE_JWT_SECRET").unwrap_or_default();
    if jwt_secret.is_empty() { return false; }

    let token = req.headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if token.is_empty() { return false; }

    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_audience(&["authenticated"]);

    decode::<Claims>(token, &DecodingKey::from_secret(jwt_secret.as_bytes()), &validation).is_ok()
}

/// Requires a valid Supabase JWT (frontend endpoints).
pub struct AuthGuard;

/// Requires either a valid x-asap-secret OR a valid Supabase JWT (shared endpoints like ride-request).
pub struct AnyAuthGuard;

impl FromRequest for AuthGuard {
    type Error = actix_web::Error;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        if verify_bearer(req) {
            ready(Ok(AuthGuard))
        } else {
            ready(Err(actix_web::error::ErrorUnauthorized("Unauthorized")))
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
            ready(Err(actix_web::error::ErrorUnauthorized("Unauthorized")))
        }
    }
}
