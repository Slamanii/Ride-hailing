mod api;
mod services;
mod db;
mod config;
mod schema;

use actix_web::{ web, App, HttpServer };
use dotenv::dotenv;
use crate::config::AppConfig;
use crate::db::init_pool; 




#[actix_web::main]
async fn main() -> std::io::Result<()> {

    dotenv().ok();
    let app_config = AppConfig::from_env();

    // Initialize DB pool
    let pool = init_pool(&app_config.database_url);



    HttpServer::new(move || {
        App::new()
        .app_data(web::Data::new(pool.clone()))
        .configure(api::init)
        .configure(services::init)
})
        .bind(("127.0.0.1", 8080))?
        .run()
        .await

}
