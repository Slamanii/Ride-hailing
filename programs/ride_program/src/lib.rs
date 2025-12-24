// Basic Anchor program skeleton for the ride-hailing app
use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};




declare_id!("Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg6Zt3b9r5Mi");

#[program]
pub mod ride_program {
    use super::*;

    pub fn record_ride(ctx: Context<RecordRide>, trip_id: [u8; 32], ride_data: RideInput) -> Result<()> {
        let ride = &mut ctx.accounts.ride_account;
        require!(!ride.is_initialized, RideError::AlreadyRecorded);
        require!(ride_data.pick_up.len() <= 128, RideError::StringTooLong);
        require!(ride_data.drop_off.len() <= 128, RideError::StringTooLong);

        ride.is_initialized = true;
        ride.passenger = ride_data.passenger;
        ride.driver = ride_data.driver;
        ride.start_ts = ride_data.start_ts_program;
        ride.end_ts = ride_data.end_ts_program;
        ride.pick_up = ride_data.pick_up;
        ride.drop_off = ride_data.drop_off;
        ride.distance_km = ride_data.distance_km;
        ride.fare_lamports = ride_data.fare_lamports_program;
        ride.fare_estimate = ride_data.fare_estimate_program;
        ride.escrow_tx_hash = ride_data.escrow_tx_hash;

        emit!(RideRecorded {
            trip_id,
            passenger: ride_data.passenger,
            driver: ride_data.driver,
            fare_lamports: ride_data.fare_lamports_program.unwrap_or(0),
            fare_estimate: ride_data.fare_estimate_program.unwrap_or(0),
        });

        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(trip_id: [u8; 32])]
pub struct RecordRide<'info> {
    #[account(
        init_if_needed,
        payer = payer,
        seeds = [b"ride", trip_id.as_ref()],
        bump,
        space = 8 + Ride::LEN
    )]
    pub ride_account: Account<'info, Ride>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[account]
pub struct Ride {
    pub is_initialized: bool,
    pub passenger: Pubkey,
    pub driver: Pubkey,
    pub start_ts: u64,
    pub end_ts: u64,
    pub pick_up: String,
    pub drop_off: String,
    pub distance_km: f64,
    pub fare_lamports: Option<u64>,
    pub fare_estimate: Option<u64>,
    pub escrow_tx_hash: [u8; 32],
}

impl Ride {
    pub const LEN: usize = 1 + 32 + 32 + 8 + 8 + 4 + 128 + 4 + 128 + 8 + 1 + 8 + 1 + 8 + 32;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct RideInput {
    pub passenger: Pubkey,
    pub driver: Pubkey,
    pub start_ts_program: u64,
    pub end_ts_program: u64,
    pub pick_up: String,
    pub drop_off: String,
    pub distance_km: f64,
    pub fare_lamports_program: Option<u64>,
    pub fare_estimate_program: Option<u64>,
    pub escrow_tx_hash: [u8; 32],
}

#[event]
pub struct RideRecorded {
    pub trip_id: [u8; 32],
    pub passenger: Pubkey,
    pub driver: Pubkey,
    pub fare_lamports: u64,
    pub fare_estimate: u64,
}

#[error_code]
pub enum RideError {
    #[msg("Ride already recorded.")]
    AlreadyRecorded,
    #[msg("String is too long.")]
    StringTooLong,
}


// Rider pays via Paystack.

// Paystack sends webhook → backend /escrow/webhook.

// escrow.rs verifies signature + payment, fetches trip & driver info.

// Backend splits payment (to treasury & driver).

// Backend constructs and signs Solana TX calling record_ride.

// Anchor program validates and stores trip data on-chain.