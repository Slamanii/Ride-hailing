use actix_cors::Cors;
use actix_web::{ http::header, middleware::Logger, web, App, HttpServer };
use dotenv::dotenv;

// The website calls the API from the browser, so its origins must be allowed; the apps aren't
// browsers and don't need this. Exact origins, never "any": the admin endpoints accept a
// signed-in user's token. CORS_ALLOWED_ORIGINS (comma-separated) replaces the defaults, e.g.
// to add a custom domain without a rebuild.
const DEFAULT_CORS_ORIGINS: [&str; 2] = ["http://localhost:3000", "https://asap-apps.vercel.app"];

fn cors_origins() -> Vec<String> {
    match std::env::var("CORS_ALLOWED_ORIGINS") {
        Ok(list) if !list.trim().is_empty() => list
            .split(',')
            .map(|o| o.trim().trim_end_matches('/').to_string())
            .filter(|o| !o.is_empty())
            .collect(),
        _ => DEFAULT_CORS_ORIGINS.iter().map(|o| o.to_string()).collect(),
    }
}

fn cors(origins: &[String]) -> Cors {
    origins
        .iter()
        .fold(Cors::default(), |cors, origin| cors.allowed_origin(origin))
        .allowed_methods(vec!["GET", "POST", "PUT", "DELETE"])
        .allowed_headers(vec![header::AUTHORIZATION, header::CONTENT_TYPE])
        .max_age(3600)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {

    println!("Starting backend...");

    dotenv().ok();
    println!("Loaded .env");
    // RUST_LOG overrides; info shows one line per request.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

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
    logic::services::invest::spawn_invest_jobs(pool.clone());
    logic::api::dashboard::spawn_performance_job(pool.clone());
    // Resumes driver searches a restart left behind.
    logic::api::riders::spawn_search_sweep(pool.clone());

    let origins = cors_origins();
    println!("CORS origins: {}", origins.join(", "));
    println!("Starting HTTP server on 0.0.0.0:8081");

    HttpServer::new(move || {
        App::new()
        // One line per request: caller, method and path, status, time. Location pings (every
        // few seconds per online driver) are left out.
        .wrap(Logger::new("%a \"%r\" %s %Dms").exclude_regex("^/matching/process-geolocation/"))
        .wrap(cors(&origins))
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
