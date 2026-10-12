use actix_web::{ web, App, HttpServer };
use dotenv::dotenv;

#[actix_web::main]
async fn main() -> std::io::Result<()> {

    println!("Starting backend...");

    dotenv().ok();
    println!("Loaded .env");

    let app_config = logic::config::AppConfig::from_env()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    println!("Loaded config");

    let pool = logic::db::init_pool(&app_config.database_url).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::ConnectionRefused, format!("DB pool creation failed: {}", e))
    })?;
    println!("Database pool initialized");

    // One handle for every worker and the background sweep.
    let pool = web::Data::new(pool);
    logic::services::korapay::spawn_stale_order_sweep(pool.clone());

    println!("Starting HTTP server on 0.0.0.0:8081");

    HttpServer::new(move || {
        App::new()
        .app_data(pool.clone())
        .configure(logic::api::errors::configure)
        .configure(logic::api::init)
        .configure(logic::services::init)
        .default_service(web::to(logic::api::errors::not_found))
    })
    .bind(("0.0.0.0", 8081))?
    .run()
    .await
}
