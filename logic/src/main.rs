use actix_web::{ web, App, HttpServer };
use dotenv::dotenv;

#[actix_web::main]
async fn main() -> std::io::Result<()> {

    println!("Starting backend...");

    dotenv().ok();
    println!("Loaded .env");

    let app_config = logic::config::AppConfig::from_env();
    println!("Loaded config");

    let pool = logic::db::init_pool(&app_config.database_url);
    println!("Database pool initialized");

    println!("Starting HTTP server on 0.0.0.0:8081");

    HttpServer::new(move || {
        App::new()
        .app_data(web::Data::new(pool.clone()))
        .configure(logic::api::init)
        .configure(logic::services::init)
    })
    .bind(("0.0.0.0", 8081))?
    .run()
    .await
}
