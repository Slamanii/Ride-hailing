use actix_web::web::ServiceConfig;

pub mod pricing;
pub mod notifications;
pub mod matching;
pub mod escrow;
pub mod paystack;
pub mod auth;
pub mod codes;
pub mod phone;
pub mod korapay;
pub mod zazu;
pub mod areas;

pub fn init(cfg: &mut ServiceConfig) {
    cfg.service(matching::routes())
       .service(areas::routes())
       .service(paystack::routes())
       .service(korapay::routes())
       .service(korapay::banks_route());
}
