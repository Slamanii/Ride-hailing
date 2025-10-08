use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::Transaction,
    pubkey::Pubkey,
    system_program,
};
use anchor_lang::{ InstructionData, ToAccountMetas };
use crate::{ api::{riders, drivers}, db::{ DbPool }, solana_client::get_anchor_program_id };
use crate::api::riders::validate_rider_account;

#[derive(Deserialize)]
pub struct PaystackWebhook {
    pub event: String,
    pub data: PaystackData,
}

#[derive(Deserialize)]
pub struct PaystackData {
    pub reference: String,
    pub amount: u64, // in kobo
    pub customer: PaystackCustomer,
}

#[derive(Deserialize)]
pub struct PaystackCustomer {
    pub email: String,
}

pub async fn handle_payment_confirmation(payload: web::Json<PaystackWebhook>, pool: web::Data<DbPool>) -> actix_web::Result<HttpResponse> {
    if payload.event != "charge.success" {
        println!("Ignoring non-success event");
        return Ok(());
    }

    let reference = &payload.data.reference;
    let amount_kobo = payload.data.amount;
    let rider_email = &payload.data.customer.email;

    // ✅ Run blocking Diesel calls in threadpool
    let pool_clone = pool.clone();
    let (trip, driver, rider) = web::block(move || {
        let mut connection = pool_clone.get().expect("Failed to get DB connection");

        let trip = riders::get_trip_by_reference(&mut connection, reference)?;
        let driver = drivers::get_driver_by_id(&mut connection, trip.driver_id)?;
        let rider = validate_rider_account(&mut connection, trip.rider_id)?;

        Ok::<_, diesel::result::Error>((trip, driver, rider))
    })
    .await
    .map_err(|e| {
        println!("DB error: {:?}", e);
        HttpResponse::InternalServerError()
    })??;

    println!("✅ Payment confirmed for trip {} by rider {}", reference, rider_email);
    println!("Trip ID: {:?}, Driver: {:?}, Rider: {:?}", trip.id, driver.id, rider.id);

    // 2️⃣ Perform Paystack split (mocked — you’d call Paystack API here)
    let driver_share = (amount_kobo as f64 * 0.8) as u64;
    let treasury_share = amount_kobo - driver_share;
    println!("Splitting ₦{:.2} — driver gets ₦{:.2}, treasury ₦{:.2}",
        amount_kobo as f64 / 100.0,
        driver_share as f64 / 100.0,
        treasury_share as f64 / 100.0,
    );

    // 3️⃣ Build the Solana transaction
    let client = RpcClient::new("https://api.devnet.solana.com".to_string());
    let payer = Keypair::from_file("~/.config/solana/id.json")?;
    let program_id = get_anchor_program_id(); // loads from IDL or env

    let ride_id = trip.ride_id; // e.g., uuid or sha256 hash bytes
    let ride_pda = Pubkey::find_program_address(&[b"ride", ride_id.as_ref()], &program_id).0;

    // Construct instruction from Anchor IDL
    let instruction = solana_sdk::instruction::Instruction {
        program_id,
        accounts: anchor_spl::accounts::RecordRide {
            ride_account: ride_pda,
            authority: payer.pubkey(),
            payer: payer.pubkey(),
            system_program: system_program::id(),
        }
        .to_account_metas(None),
        data: anchor_spl::instruction::RecordRide {
            ride_id,
            ride_data: anchor_spl::types::RideInput {
                passenger: trip.rider_pubkey,
                driver: driver.wallet_pubkey,
                start_ts: trip.start_ts,
                end_ts: trip.end_ts,
                start_lat: trip.start_lat,
                start_lon: trip.start_lon,
                end_lat: trip.end_lat,
                end_lon: trip.end_lon,
                distance_m: trip.distance_m,
                fare_lamports: trip.fare_lamports,
                escrow_tx_hash: reference.as_bytes()[..32].try_into().unwrap_or([0u8;32]),
            }
        }.data(),
    };

    // Sign and send transaction
    let blockhash = client.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );
    client.send_and_confirm_transaction(&tx)?;

    println!("Ride recorded on-chain for reference {}", reference);

    Ok(HttpResponse::Ok().json({
        serde_json::json!({
            "status": "success",
            "trip_id": trip.id,
            "rider": rider_email,
            "driver": driver.name,
            "amount_kobo": amount_kobo
        })
   }))
}
