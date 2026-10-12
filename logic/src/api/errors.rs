use actix_web::{ web, HttpRequest, HttpResponse, error::InternalError, http::StatusCode };
use serde_json::{ json, Value };

// Every reply is JSON. Errors are non-2xx with { "error": "<readable message>" }.
pub fn error_body(msg: impl std::fmt::Display) -> Value {
    json!({ "error": msg.to_string() })
}

pub fn message_body(msg: impl std::fmt::Display) -> Value {
    json!({ "message": msg.to_string() })
}

pub fn json_error(status: StatusCode, msg: impl std::fmt::Display) -> HttpResponse {
    HttpResponse::build(status).json(error_body(msg))
}

// For extractors: an actix error that still renders as a JSON body.
pub fn json_extractor_error(status: StatusCode, msg: impl std::fmt::Display) -> actix_web::Error {
    InternalError::from_response(msg.to_string(), json_error(status, msg)).into()
}

fn bad_request(err: impl std::fmt::Display) -> actix_web::Error {
    json_extractor_error(StatusCode::BAD_REQUEST, err)
}

// Malformed bodies, paths and query strings come back as 400 JSON instead of plain text.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.app_data(web::JsonConfig::default().error_handler(|err, _| bad_request(format!("Invalid JSON body: {}", err))))
       .app_data(web::PathConfig::default().error_handler(|err, _| bad_request(format!("Invalid path: {}", err))))
       .app_data(web::QueryConfig::default().error_handler(|err, _| bad_request(format!("Invalid query: {}", err))));
}

pub async fn not_found(req: HttpRequest) -> HttpResponse {
    json_error(StatusCode::NOT_FOUND, format!("No route for {} {}", req.method(), req.path()))
}

// Error type for handlers whose work runs on the blocking DB pool.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    PaymentRequired(String),
    BadGateway(String),
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::PaymentRequired(_) => StatusCode::PAYMENT_REQUIRED,
            ApiError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            ApiError::BadRequest(m) | ApiError::Forbidden(m) | ApiError::NotFound(m) | ApiError::Conflict(m)
            | ApiError::PaymentRequired(m) | ApiError::BadGateway(m) | ApiError::Internal(m) => m,
        }
    }

    pub fn response(&self) -> HttpResponse {
        if let ApiError::Internal(m) = self {
            eprintln!("Internal error: {}", m);
        }
        json_error(self.status(), self.message())
    }
}

impl From<diesel::result::Error> for ApiError {
    fn from(e: diesel::result::Error) -> Self {
        use diesel::result::{ Error, DatabaseErrorKind };
        match &e {
            Error::DatabaseError(DatabaseErrorKind::UniqueViolation, info) =>
                ApiError::Conflict(format!("Already exists: {}", info.message())),
            Error::DatabaseError(DatabaseErrorKind::ForeignKeyViolation, info) =>
                ApiError::BadRequest(format!("Unknown reference: {}", info.message())),
            Error::DatabaseError(DatabaseErrorKind::CheckViolation, info) =>
                ApiError::BadRequest(format!("Rejected by the database: {}", info.message())),
            _ => ApiError::Internal(format!("DB error: {}", e)),
        }
    }
}

// Runs `f` on the blocking pool with a pooled connection.
pub async fn blocking<T, F>(pool: &web::Data<crate::db::DbPool>, f: F) -> Result<T, ApiError>
where
    F: FnOnce(&mut diesel::pg::PgConnection) -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    let pool = pool.clone();
    web::block(move || {
        let mut conn = pool.get().map_err(|e| ApiError::Internal(format!("DB pool error: {}", e)))?;
        f(&mut conn)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Blocking error: {}", e)))?
}
