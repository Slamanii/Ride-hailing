use std::env;
use solana_sdk::pubkey::Pubkey;

pub struct AppConfig {
    pub database_url: String,
    pub secret_key: String,
    pub program_id: Pubkey,
    pub solana_rpc_url: String,
}

impl AppConfig {
    pub fn from_env() -> Self {
        dotenv::dotenv().ok(); // load .env

        Self {
            database_url: env::var("DATABASE_URL").expect("DATABASE_URL must be set"),
            secret_key: env::var("PAYSTACK_SECRET").expect("PAYSTACK_SECRET must be set"),
            program_id: env::var("SOLANA_PROGRAM_ID")
                .expect("SOLANA_PROGRAM_ID must be set")
                .parse()
                .expect("Invalid program ID"),
            solana_rpc_url: env::var("SOLANA_RPC_URL")
                .unwrap_or_else(|_| "https://api.devnet.solana.com".into()),
        }
    }
}
