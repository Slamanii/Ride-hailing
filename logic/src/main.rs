use actix_web::{get, web, App, HttpServer, Responder};
use dotenvy::dotenv;
use std::env;
mod api;

#[get("/health")]
async fn health() -> impl Responder {
    "Backend is alive 🚀"
}

#[tokio::main]
async fn main() -> std::io::Result<()> {

    dotenv().ok();

    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = logic::db::init_pool(db_url);


    HttpServer::new(|| {
        App::new()
        .app_data(web::Data::new(pool.clone()))
        .configure(api::init)
})
        .bind(("127.0.0.1", 8080))?
        .run()
        .await

}
