use std::env;

// What the server needs to start. Payment and Solana settings are read where they're used
// (korapay.rs, escrow.rs), so a missing one fails that call instead of the whole server.
pub struct AppConfig {
    pub database_url: String,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, String> {
        dotenv::dotenv().ok(); // load .env

        let database_url = env::var("DATABASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or("DATABASE_URL must be set")?;
        Ok(Self { database_url })
    }
}
