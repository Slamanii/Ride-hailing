pub mod pricing;
pub mod notifications;
pub mod matching;
pub mod escrow;
pub mod solana_client;
pub mod paystack;


pub fn init(cfg: &mut web::ServiceConfig) {
    cfg.service(escrow::routes())
       .service(matching::routes())
       .service(notifications::routes())
       .service(pricing::routes());
       .service(paysrack::routes());
       .service(solana_client::routes());

}