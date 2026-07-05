//! create-alt — one-shot utility that resolves every pool in pools.json and
//! packs all recurring accounts into a fresh Address Lookup Table, printing
//! the table address to put into LOOKUP_TABLES. Required for 3-4 hop routes
//! (raw v0 transactions overflow the 1232-byte packet without one).
//!
//! Usage: KEYPAIR_PATH=... RPC_ENDPOINT=... create-alt [path/to/pools.json]

use anyhow::{Context, Result};
use arb_common::ix::DexKind;
use arb_common::opportunity::OpportunityHop;
use arb_executor::resolver::{derive_ata, Resolver, WSOL_MINT};
use arbitrage_program::TOKEN_PROGRAM;
use serde::Deserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::instruction::{create_lookup_table, extend_lookup_table},
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::read_keypair_file,
    signer::Signer,
    transaction::Transaction,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

#[derive(Deserialize)]
struct PoolsFile {
    pools: Vec<PoolEntry>,
}

#[derive(Deserialize)]
struct PoolEntry {
    address: String,
    dex: String,
    #[serde(default)]
    label: Option<String>,
}

const EXTEND_CHUNK: usize = 20;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt().init();

    let pools_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "pools.json".to_string());
    let keypair_path = std::env::var("KEYPAIR_PATH").context("KEYPAIR_PATH required")?;
    let rpc_url = std::env::var("RPC_ENDPOINT")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    let payer = read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("read keypair {keypair_path}: {e}"))?;
    let rpc = Arc::new(RpcClient::new_with_commitment(
        rpc_url,
        CommitmentConfig::confirmed(),
    ));
    let resolver = Resolver::new(rpc.clone(), payer.pubkey(), Duration::from_secs(60));

    let file: PoolsFile = serde_json::from_str(
        &std::fs::read_to_string(&pools_path).with_context(|| format!("read {pools_path}"))?,
    )?;

    // Resolve every pool in both directions to sweep every account the
    // executor could ever reference, including per-direction tick arrays.
    let mut addresses: BTreeSet<Pubkey> = BTreeSet::new();
    addresses.insert(TOKEN_PROGRAM);
    addresses.insert(payer.pubkey());
    addresses.insert(derive_ata(&payer.pubkey(), &WSOL_MINT));
    for tip in arb_executor::jito::JITO_TIP_ACCOUNTS {
        addresses.insert(tip);
    }

    for entry in &file.pools {
        let dex = match entry.dex.as_str() {
            "raydium-v4" => DexKind::RaydiumV4,
            "orca-whirlpool" => DexKind::OrcaWhirlpool,
            other => {
                eprintln!("skipping {}: unknown dex {other}", entry.address);
                continue;
            }
        };
        let pool_pk: Pubkey = entry.address.parse().context("bad pool address")?;
        let (mint_a, mint_b) = resolver.pool_mints(pool_pk, dex).await?;

        // Resolve BOTH directions: whirlpool tick arrays fan out in the
        // trade direction, so each side references a different triple.
        let mut count = 0usize;
        for (input, output) in [(mint_a, mint_b), (mint_b, mint_a)] {
            let hop = OpportunityHop {
                pool: entry.address.clone(),
                dex,
                input_mint: input.to_string(),
                output_mint: output.to_string(),
                amount_in: 0,
                expected_amount_out: 0,
                min_amount_out: 0,
            };
            let resolved = resolver.resolve_hop(&hop).await?;
            for meta in &resolved.metas {
                addresses.insert(meta.pubkey);
            }
            addresses.insert(derive_ata(&payer.pubkey(), &input));
            count = resolved.metas.len();
        }
        println!(
            "resolved {} ({}) -> {count} accounts/direction",
            entry.address,
            entry.label.as_deref().unwrap_or("-"),
        );
    }

    println!("total unique addresses for ALT: {}", addresses.len());
    let addresses: Vec<Pubkey> = addresses.into_iter().collect();

    // Create the table against a recent slot.
    let recent_slot = rpc.get_slot().await? - 1;
    let (create_ix, table_address) =
        create_lookup_table(payer.pubkey(), payer.pubkey(), recent_slot);
    let blockhash = rpc.get_latest_blockhash().await?;
    let tx = Transaction::new_signed_with_payer(
        &[create_ix],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&tx)
        .await
        .context("create lookup table")?;
    println!("created lookup table: {table_address}");

    for chunk in addresses.chunks(EXTEND_CHUNK) {
        let extend_ix = extend_lookup_table(
            table_address,
            payer.pubkey(),
            Some(payer.pubkey()),
            chunk.to_vec(),
        );
        let blockhash = rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(
            &[extend_ix],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        );
        rpc.send_and_confirm_transaction(&tx)
            .await
            .context("extend lookup table")?;
        println!("extended with {} addresses", chunk.len());
    }

    println!("\nDone. Set in .env:\nLOOKUP_TABLES={table_address}");
    Ok(())
}
