use anchor::prelude::*;
use anchor_client::solana_client::rpc_client::RpcClient;
use anchor_client::solana_sdk::signature::{Keypair, Signer};
use anchor_client::solana_sdk::commitment_config::CommitmentConfig;
use anchor_client::Client;
use std::rc::Rc;

pub mod admin;
pub mod riders;
pub mod drivers;

use actix_web::{self, ServiceConfig};

pub fn init(cfg: &mut ServiceConfig) {
    cfg.service(admin::routes())
        .service(riders::routes())
        .service(drivers::routes());
}