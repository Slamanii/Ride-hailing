use actix_web::web;
use sha2::{ Sha256, Digest };
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    signature::{ Keypair, read_keypair_file, Signer },
    transaction::Transaction,
    pubkey::Pubkey,
    system_program,
};
use anyhow::{ Result, anyhow };
use std::str::FromStr;
use std::env;
use uuid::Uuid;
use ride_program::accounts::RecordRide;
use ride_program::instruction::RecordRide as RecordRideIx;
use ride_program::RideInput;
use anchor_client::anchor_lang::{ InstructionData, ToAccountMetas };
use crate::api::errors::blocking;
use crate::api::trips::{ Trip, get_trip_by_order_ref };
use crate::db::DbPool;
use crate::services::korapay::{ self, TransferStatus };

// Records completed trips on-chain (ride_program's RecordRide), driven by Korapay: an app trip
// is recorded only once Korapay confirms the driver's payout, and its escrow_tx_hash is the
// sha256 of that payout's reference. A Zazu trip (Zazu pays its drivers) is recorded at
// drop-off with the hash of the trip's reference.
//
// Off unless ONCHAIN_RECORDING=on: trips complete either way, the record is an extra. A record
// that fails is logged and not retried; the program refuses a second record of the same trip.

fn recording_enabled() -> bool {
    env::var("ONCHAIN_RECORDING").map_or(false, |v| v.trim().eq_ignore_ascii_case("on"))
}

fn pubkey_from_string(s: &str) -> Result<Pubkey> {
    Pubkey::from_str(s).map_err(|e| anyhow!(e))
}

pub fn i64_to_u64(v: i64) -> Result<u64> {
    if v < 0 {
        Err(anyhow!("negative value cannot convert to u64"))
    } else {
        Ok(v as u64)
    }
}

pub fn vec_to_array_32(v: Vec<u8>) -> Result<[u8; 32]> {
    if v.len() != 32 {
        return Err(anyhow!("expected 32 bytes, got {}", v.len()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&v);
    Ok(arr)
}

fn sha256_32(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn program_id() -> Result<Pubkey> {
    let id = env::var("SOLANA_PROGRAM_ID").map_err(|_| anyhow!("SOLANA_PROGRAM_ID is not set"))?;
    pubkey_from_string(id.trim()).map_err(|e| anyhow!("SOLANA_PROGRAM_ID is invalid: {}", e))
}

// SOLANA_KEYPAIR_PATH, else the Solana CLI default (~/.config/solana/id.json).
fn payer() -> Result<Keypair> {
    let path = match env::var("SOLANA_KEYPAIR_PATH") {
        Ok(path) if !path.trim().is_empty() => path,
        _ => {
            let home = env::var("HOME").map_err(|_| anyhow!("Neither SOLANA_KEYPAIR_PATH nor HOME is set"))?;
            format!("{}/.config/solana/id.json", home)
        }
    };
    read_keypair_file(&path).map_err(|e| anyhow!("Could not load the payer keypair from {}: {}", path, e))
}

fn ride_input(trip: &Trip, escrow_reference: &str) -> Result<([u8; 32], RideInput)> {
    let end_ts = trip.end_ts.ok_or_else(|| anyhow!("trip has no end_ts"))?;
    let trip_id = vec_to_array_32(trip.trip_id.clone())?;
    let key = |value: &Option<String>, whose: &str| -> Result<Pubkey> {
        let value = value.as_deref().ok_or_else(|| anyhow!("trip has no {} pubkey", whose))?;
        pubkey_from_string(value).map_err(|e| anyhow!("invalid {} pubkey: {}", whose, e))
    };
    Ok((trip_id, RideInput {
        passenger: key(&trip.rider_pubkey, "rider")?,
        driver: key(&trip.driver_pubkey, "driver")?,
        start_ts_program: i64_to_u64(trip.start_ts)?,
        end_ts_program: i64_to_u64(end_ts)?,
        pick_up: trip.pick_up.to_string(),
        drop_off: trip.drop_off.to_string(),
        distance_km: trip.distance_km,
        fare_lamports_program: trip.fare_lamports.map(i64_to_u64).transpose()?,
        fare_estimate_program: trip.fare_estimate.map(i64_to_u64).transpose()?,
        escrow_tx_hash: sha256_32(escrow_reference),
    }))
}

// Blocking: Solana RPC. Returns the transaction signature, or None if the trip was already
// recorded.
fn send_record(trip_id: [u8; 32], ride_data: RideInput) -> Result<Option<String>> {
    let rpc_url = env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "https://api.devnet.solana.com".into());
    let client = RpcClient::new(rpc_url);
    let payer = payer()?;
    let program_id = program_id()?;

    let (ride_pda, _) = Pubkey::find_program_address(&[b"ride", &trip_id], &program_id);
    if client.get_account(&ride_pda).is_ok() {
        return Ok(None);
    }

    let instruction = solana_sdk::instruction::Instruction {
        program_id,
        accounts: RecordRide {
            ride_account: ride_pda,
            authority: payer.pubkey(),
            payer: payer.pubkey(),
            system_program: system_program::id(),
        }
        .to_account_metas(None),
        data: RecordRideIx { trip_id, ride_data }.data(),
    };

    let blockhash = client.get_latest_blockhash().map_err(|e| anyhow!("RPC error: {}", e))?;
    let tx = Transaction::new_signed_with_payer(&[instruction], Some(&payer.pubkey()), &[&payer], blockhash);
    let signature = client.send_and_confirm_transaction(&tx).map_err(|e| anyhow!("transaction failed: {}", e))?;
    Ok(Some(signature.to_string()))
}

// `payout_reference`: the Korapay payout that completed an app trip; None for a Zazu trip.
async fn record_ride(pool: &web::Data<DbPool>, order_ref: Uuid, payout_reference: Option<String>) -> Result<Option<String>> {
    let trip = blocking(pool, move |conn| Ok(get_trip_by_order_ref(conn, order_ref)?))
        .await
        .map_err(|e| anyhow!(e.message().to_string()))?
        .ok_or_else(|| anyhow!("no trip record"))?;
    if trip.status != "Completed" {
        return Err(anyhow!("trip is {}, not Completed", trip.status));
    }

    let escrow_reference = match payout_reference {
        Some(reference) => {
            match korapay::query_payout(&reference).await.map_err(|e| anyhow!(e))? {
                TransferStatus::Success => reference,
                other => return Err(anyhow!("Korapay reports payout {} as {:?}", reference, other)),
            }
        }
        None => trip.reference.clone(),
    };

    let (trip_id, ride_data) = ride_input(&trip, &escrow_reference)?;
    tokio::task::spawn_blocking(move || send_record(trip_id, ride_data))
        .await
        .map_err(|e| anyhow!("blocking task failed: {}", e))?
}

// Records a trip that was just completed, in the background. Does nothing unless
// ONCHAIN_RECORDING=on.
pub fn spawn_record_ride(pool: web::Data<DbPool>, order_ref: Uuid, payout_reference: Option<String>) {
    if !recording_enabled() {
        return;
    }
    tokio::spawn(async move {
        match record_ride(&pool, order_ref, payout_reference).await {
            Ok(Some(signature)) => println!("Trip {} recorded on-chain: {}", order_ref, signature),
            Ok(None) => println!("Trip {} was already recorded on-chain", order_ref),
            Err(e) => eprintln!("Trip {}: on-chain record failed: {}", order_ref, e),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escrow_hash_is_sha256_of_the_reference() {
        let hash = sha256_32("dr_asap_x");
        assert_eq!(hash.len(), 32);
        assert_eq!(hash, sha256_32("dr_asap_x"));
        assert_ne!(hash, sha256_32("dr_asap_y"));
    }
}
