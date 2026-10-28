use actix_web::web::ServiceConfig;

pub mod admin;
pub mod riders;
pub mod drivers;
pub mod trips;
pub mod orders;
pub mod errors;
pub mod applications;
pub mod disputes;
pub mod dashboard;
pub mod profile;


pub fn init(cfg: &mut ServiceConfig) {
    cfg.service(admin::routes())
       .service(riders::routes())
       .service(drivers::routes())
       .service(trips::routes())
       .service(orders::routes());
}
