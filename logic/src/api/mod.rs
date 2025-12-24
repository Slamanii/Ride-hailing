use actix_web::web::ServiceConfig;

pub mod admin;
pub mod riders;
pub mod drivers;
pub mod trips;


pub fn init(cfg: &mut ServiceConfig) {
    cfg.service(admin::routes())
       .service(riders::routes())
       .service(drivers::routes())
       .service(trips::routes());
}