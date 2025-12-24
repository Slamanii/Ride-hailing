use actix_web::web::ServiceConfig;

pub mod pricing;
pub mod notifications;
pub mod matching;
pub mod escrow;
pub mod paystack;

pub fn init(cfg: &mut ServiceConfig) {
    cfg.service(escrow::routes())
       .service(matching::routes())
       .service(paystack::routes());
}
