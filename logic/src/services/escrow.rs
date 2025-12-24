use actix_web::{web, post, Scope, HttpResponse};
use serde::{ Deserialize, Serialize };
use sha2::{Sha256, Digest};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    signature::{ Keypair, read_keypair_file, Signer },
    transaction::Transaction,
    pubkey::Pubkey,
    system_program,
};
use anyhow::{ Result, anyhow };
use std::str::FromStr;
use ride_program::accounts::RecordRide;
use ride_program::instruction::RecordRide as RecordRideIx;
use ride_program::RideInput;
use anchor_client::anchor_lang::{ InstructionData, ToAccountMetas };
use crate::{ api::{ riders, drivers }, db::{ DbPool } };
use crate::api::trips::get_trip_by_reference;
use crate::schema::trips::dsl::*;
use std::env;


 fn pubkey_from_string(s: &str) -> Result<Pubkey> {
    Pubkey::from_str(s).map_err(|e| anyhow!(e))
}


fn i64_to_u64(v: i64) -> Result<u64> {
    if v < 0 {
        Err(anyhow!("negative value cannot convert to u64"))
    } else {
        Ok(v as u64)
    }
}

fn vec_to_array_32(v: Vec<u8>) -> Result<[u8; 32]> {
    if v.len() != 32 {
        return Err(anyhow!("expected 32 bytes, got {}", v.len()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&v);
    Ok(arr)
}

fn get_program_id() -> Pubkey {
    let program_id_str =
        std::env::var("SOLANA_PROGRAM_ID").expect("SOLANA_PROGRAM_ID must be set");
    program_id_str
        .parse::<Pubkey>()
        .expect("Invalid program ID")
}


pub async fn handle_payment_confirmation(
    payload: web::Json<PaystackWebhook>,
    pool: web::Data<DbPool>,
) -> HttpResponse {
    // 1️⃣ Ignore non-success events
    if payload.event != "charge.success" {
        return HttpResponse::Ok().body("Ignoring non-success event");
    }

    let trip_reference = payload.data.reference.clone();
    let amount_kobo = payload.data.amount;
    let rider_email_stack = payload.data.customer.email.clone();

    // 2️⃣ Paystack split (OFF-CHAIN)
    let driver_share = (amount_kobo * 80) / 100;
    let treasury_share = amount_kobo - driver_share;

    println!(
        "💰 Paystack split — Driver: ₦{}, Treasury: ₦{}",
        driver_share as f64 / 100.0,
        treasury_share as f64 / 100.0,
    );

    // Paystack split (ON-CHAIN)
    // This is where you would call Paystack’s split/subaccount API.
    // We do NOT put Paystack HTTP calls inside web::block.
    //
    // Example (pseudo):
    // paystack::split_payment(reference, driver_share, treasury_share).await?;





    let trip = match web::block({
        let pool = pool.clone();
        let trip_reference = trip_reference.clone();

        move || {
            let mut conn = pool.get().expect("Failed to get connection");
            get_trip_by_reference(&mut conn, &trip_reference)
        }
    })
    .await
    {
        Ok(Ok(trip)) => trip,

        Ok(Err(e)) => {
            eprintln!("DB error: {:?}", e);
            return HttpResponse::InternalServerError().body("Database error");
        }

        Err(e) => {
            eprintln!("Threadpool error: {:?}", e);
            return HttpResponse::InternalServerError().body("Threadpool error");
        }
    };

    println!(
        "✅ Payment confirmed for trip {} by rider {}",
        trip_reference, rider_email_stack
    );

    // 4️⃣ Convert DB → Solana-safe types
    let passenger = match pubkey_from_string(&trip.rider_pubkey) {
        Ok(v) => v,
        Err(_) => return HttpResponse::BadRequest().body("Invalid rider pubkey"),
    };

    let driver = match pubkey_from_string(&trip.driver_pubkey) {
        Ok(v) => v,
        Err(_) => return HttpResponse::BadRequest().body("Invalid driver pubkey"),
    };

    let start_ts_program = match i64_to_u64(trip.start_ts) {
        Ok(v) => v,
        Err(_) => return HttpResponse::BadRequest().body("Invalid start_ts"),
    };

    let end_ts_program: u64 = match trip.end_ts {
    Some(v) => match i64_to_u64(v) {
        Ok(v) => v,
        Err(_) => return HttpResponse::BadRequest().body("Invalid end_ts"),
    },
    None => return HttpResponse::BadRequest().body("end_ts is required"),
};


    let fare_lamports_program = match trip
        .fare_lamports
        .map(i64_to_u64)
        .transpose()
    {
        Ok(v) => v,
        Err(_) => return HttpResponse::BadRequest().body("Invalid fare"),
    };

    let fare_estimate_program = match trip
        .fare_estimate
        .map(i64_to_u64)
        .transpose()
    {
        Ok(v) => v,
        Err(_) => return HttpResponse::BadRequest().body("Invalid fare estimate"),
    };

    let trip_id_program = match vec_to_array_32(trip.trip_id) {
        Ok(v) => v,
        Err(_) => return HttpResponse::BadRequest().body("Invalid trip_id"),
    };

    // 5️⃣ Solana setup
    let rpc_url = std::env::var("SOLANA_RPC_URL")
        .unwrap_or_else(|_| "https://api.devnet.solana.com".into());
    let client = RpcClient::new(rpc_url);

    let payer_path = std::env::var("HOME").unwrap() + "/.config/solana/id.json";
    let payer: Keypair = read_keypair_file(&payer_path)
                        .expect("Keypair load failed");
    

    let program_id = get_program_id();

    let seeds: &[&[u8]] = &[b"ride", &trip_id_program];

    let ride_pda =
        Pubkey::find_program_address(seeds, &program_id).0;

    let escrow_tx_hash = {
        let mut hasher = Sha256::new();
        hasher.update(trip_reference.as_bytes());
        let result = hasher.finalize();
        <[u8; 32]>::try_from(result.as_slice()).unwrap()
    };

    // 6️⃣ Build instruction
    let instruction = solana_sdk::instruction::Instruction {
        program_id,
        accounts: RecordRide {
            ride_account: ride_pda,
            authority: payer.pubkey(),
            payer: payer.pubkey(),
            system_program: system_program::id(),
        }
        .to_account_metas(None),
        data: RecordRideIx {
            trip_id: trip_id_program,
            ride_data: RideInput {
                passenger,
                driver,
                start_ts_program,
                end_ts_program,
                pick_up: trip.pick_up,
                drop_off: trip.drop_off,
                distance_km: trip.distance_km,
                fare_lamports_program,
                fare_estimate_program,
                escrow_tx_hash,
            },
        }
        .data(),
    };

    // 7️⃣ Send transaction
    let blockhash = match client.get_latest_blockhash() {
        Ok(h) => h,
        Err(_) => return HttpResponse::InternalServerError().body("RPC error"),
    };

    let tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );

    if let Err(e) = client.send_and_confirm_transaction(&tx) {
        eprintln!("Transaction failed: {:?}", e);
        return HttpResponse::InternalServerError().body("Transaction failed");
    }

    println!("Ride recorded on-chain for reference {}", trip_reference);

    HttpResponse::Ok().json(serde_json::json!({
            "status": "success",
            "trip_id": hex::encode(trip_id_program),
            "rider": rider_email_stack,
            "amount_kobo": amount_kobo
    }))
}




pub fn routes() -> Scope {
    web::scope("/escrow")
        .route("/api/paystack/webhook", web::post().to(handle_payment_confirmation))
}



#[derive(Debug, Serialize, Deserialize)]
pub struct PaystackWebhook {
    pub event: String,
    pub data: PaystackData,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaystackData {
    pub reference: String,
    pub amount: u64, // in kobo
    pub currency: String,
    pub id: i64,
    pub status: String,
    pub customer: PaystackCustomer,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaystackCustomer {
    pub email: String,
}


//write paystack post json with reference



             //For transfer of cngn via fiat payment
// so the backend should generate the metadata and parse an order via hook to the bank with an amount and expected order_id if both 
// match accurately it responds with success to the backend, the backend then creates a transaction and sends to the treasury program 
// containning the destination address and amount the key here is that the backend efficently generates the order_id and keeps a reference
//  to the user publickey for that id (obviously with a time duration for validity of order_id) 
