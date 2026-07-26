//! S15A / Phase 0.5c — READ-ONLY reconstruction of specific arbitrage routes.
//!
//! Takes a list of signatures (the profitable `meteora-dlmm+orca-whirlpool`
//! set from the widened scan) and answers, per transaction, the three questions
//! that decide whether OUR infrastructure could have traded them:
//!
//!   1. **Shape** — how many REAL swap hops (top-level CPI into a DEX program,
//!      de-duplicated from nested inner instructions), in what order, and is it
//!      a closed WSOL round trip?
//!   2. **Markets** — exact pool addresses and the exact non-WSOL token mints.
//!   3. **Safety** — does every token mint pass our own `mint_safety` screen
//!      (classic SPL, no mint/freeze authority, no Token-2022 extensions)?
//!
//! Plus: direct-vs-CPI execution, flash-loan / temporary-liquidity markers, and
//! the arb program used (if the operator runs their own on-chain executor).
//!
//! NEVER builds, signs, simulates or submits. Read-only RPC.

use anyhow::{Context, Result};
use arb_monitor::mint_safety::screen_mint;
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status_client_types::UiTransactionEncoding;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::str::FromStr;

const WSOL: &str = "So11111111111111111111111111111111111111112";

const DEX_PROGRAMS: &[(&str, &str)] = &[
    ("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", "raydium-v4"),
    (
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",
        "raydium-clmm",
    ),
    (
        "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C",
        "raydium-cpmm",
    ),
    (
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
        "orca-whirlpool",
    ),
    (
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
        "meteora-dlmm",
    ),
    (
        "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB",
        "meteora-pools",
    ),
    ("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", "pump-amm"),
    ("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", "jupiter-v6"),
    ("ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY", "zerofi"),
    ("SoLFiHG9TfgtdUXUjWAxi3LtvYuFyDLVhBWxdMZxyCe", "solfi"),
];

/// Programs that indicate borrowed / temporary liquidity in the transaction.
const FLASH_MARKERS: &[(&str, &str)] = &[
    ("So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo", "solend"),
    ("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD", "kamino-lend"),
    ("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA", "marginfi"),
    (
        "FLASH6Lo6h3iasJKWDs2F8TkW2UKf3s15C8PMGuVfgBn",
        "flash-trade",
    ),
];

fn dex_of(p: &str) -> Option<&'static str> {
    DEX_PROGRAMS.iter().find(|(k, _)| *k == p).map(|(_, v)| *v)
}
fn flash_of(p: &str) -> Option<&'static str> {
    FLASH_MARKERS.iter().find(|(k, _)| *k == p).map(|(_, v)| *v)
}

/// One reconstructed swap hop: a DEX invocation at ANY nesting depth, recorded
/// with the depth so top-level hops can be separated from nested CPI noise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hop {
    pub venue: String,
    /// "outer:<i>" or "inner:<outer>.<j>".
    pub location: String,
    /// 1 = top-level instruction, 2+ = inner CPI.
    pub depth: u8,
    /// Best-effort pool address: the account in the hop that is owned by the
    /// DEX program (resolved via a batched owner lookup).
    pub pool: Option<String>,
    pub n_accounts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconTx {
    pub sig: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub signer: String,
    /// Distinct DEX venues used.
    pub venues: Vec<String>,
    /// Every DEX invocation found, ordered.
    pub hops: Vec<Hop>,
    /// Hops at depth 1 (what an executor would have to build itself).
    pub top_level_hops: usize,
    /// Hops nested inside another program (i.e. an on-chain router did them).
    pub nested_hops: usize,
    /// The non-system program that owns the transaction's top-level logic, if
    /// it is not a DEX itself — i.e. the operator's own arb program.
    pub custom_programs: Vec<String>,
    /// Flash-loan / temporary-liquidity providers detected.
    pub flash_markers: Vec<String>,
    /// Non-WSOL token mints that moved in this transaction.
    pub token_mints: Vec<String>,
    /// Number of distinct token mints touched (2 = simple WSOL↔TOKEN cycle).
    pub distinct_mints: usize,
    pub net_profit: i128,
    pub compute_units: Option<u64>,
    pub n_account_keys: usize,
    pub used_alt: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct MintScreen {
    pub mint: String,
    pub occurrences: u64,
    pub verdict: String,
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    let args: Vec<String> = std::env::args().collect();
    let sig_file = args
        .iter()
        .position(|a| a == "--sigs")
        .and_then(|i| args.get(i + 1))
        .context("--sigs <file with one signature per line> required")?;
    let out = std::env::var("RECON_OUT").unwrap_or_else(|_| "reports/forensic".into());
    std::fs::create_dir_all(&out).ok();

    let body = std::fs::read_to_string(sig_file).context("read signature file")?;
    let sigs: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    println!("=== S15A / Phase 0.5c route reconstruction (READ-ONLY) ===");
    println!("transactions to reconstruct: {}\n", sigs.len());

    let mut recons: Vec<ReconTx> = Vec::new();
    let mut pool_candidates: BTreeSet<String> = BTreeSet::new();
    let mut mint_counts: BTreeMap<String, u64> = BTreeMap::new();

    for s in &sigs {
        match recon_one(&rpc, s) {
            Ok(r) => {
                for h in &r.hops {
                    if let Some(p) = &h.pool {
                        pool_candidates.insert(p.clone());
                    }
                }
                for m in &r.token_mints {
                    *mint_counts.entry(m.clone()).or_default() += 1;
                }
                recons.push(r);
            }
            Err(e) => println!("  {}… reconstruction failed: {e}", &s[..12]),
        }
    }

    // ── shape summary ──
    println!("═══ ROUTE SHAPE ═══");
    let mut shape: BTreeMap<(usize, usize), u64> = BTreeMap::new();
    for r in &recons {
        *shape.entry((r.top_level_hops, r.nested_hops)).or_default() += 1;
    }
    println!("(top-level hops, nested hops) → count");
    for ((t, n), c) in &shape {
        println!("   ({t}, {n}) → {c}");
    }
    let two_hop = recons
        .iter()
        .filter(|r| r.hops.len() == 2 && r.distinct_mints <= 2)
        .count();
    println!(
        "\nsimple 2-hop, single-token cycles (what our engine can express): {two_hop}/{}",
        recons.len()
    );
    let mut mints_dist: BTreeMap<usize, u64> = BTreeMap::new();
    for r in &recons {
        *mints_dist.entry(r.distinct_mints).or_default() += 1;
    }
    println!("distinct non-WSOL mints per tx: {mints_dist:?}");
    let mut acct_keys: Vec<usize> = recons.iter().map(|r| r.n_account_keys).collect();
    acct_keys.sort_unstable();
    if !acct_keys.is_empty() {
        println!(
            "account keys per tx: min={} median={} max={}   ALT used: {}/{}",
            acct_keys[0],
            acct_keys[acct_keys.len() / 2],
            acct_keys[acct_keys.len() - 1],
            recons.iter().filter(|r| r.used_alt).count(),
            recons.len()
        );
    }

    // ── custom executor programs / flash loans ──
    let mut custom: BTreeMap<String, u64> = BTreeMap::new();
    for r in &recons {
        for c in &r.custom_programs {
            *custom.entry(c.clone()).or_default() += 1;
        }
    }
    println!("\n═══ OPERATOR INFRASTRUCTURE ═══");
    println!("custom (non-DEX) top-level programs — i.e. their own arb executor:");
    let mut cv: Vec<(&String, &u64)> = custom.iter().collect();
    cv.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    for (p, c) in cv.iter().take(8) {
        println!("   {p}  n={c}");
    }
    let flash: u64 = recons
        .iter()
        .filter(|r| !r.flash_markers.is_empty())
        .count() as u64;
    println!(
        "transactions using flash/temporary liquidity: {flash}/{}",
        recons.len()
    );

    // ── mint safety screen (the decisive filter) ──
    println!("\n═══ MINT SAFETY (our own screen) ═══");
    let mint_keys: Vec<Pubkey> = mint_counts
        .keys()
        .filter_map(|m| Pubkey::from_str(m).ok())
        .collect();
    let mut screens: Vec<MintScreen> = Vec::new();
    let mut pass = 0u64;
    let mut fail: BTreeMap<String, u64> = BTreeMap::new();
    for chunk in mint_keys.chunks(100) {
        let accs = rpc.get_multiple_accounts(chunk).unwrap_or_default();
        for (k, acc) in chunk.iter().zip(accs) {
            let key = k.to_string();
            let occ = mint_counts.get(&key).copied().unwrap_or(0);
            let verdict = match acc {
                Some(a) => match screen_mint(&a.owner.to_string(), &a.data) {
                    Ok(()) => {
                        pass += 1;
                        "PASS".to_string()
                    }
                    Err(e) => {
                        let v = format!("{e:?}");
                        let short = v.split(' ').next().unwrap_or(&v).to_string();
                        *fail.entry(short.clone()).or_default() += 1;
                        v
                    }
                },
                None => {
                    *fail.entry("MissingAccount".into()).or_default() += 1;
                    "MissingAccount".into()
                }
            };
            screens.push(MintScreen {
                mint: key,
                occurrences: occ,
                verdict,
            });
        }
    }
    println!(
        "distinct non-WSOL mints traded: {}   PASS our screen: {pass}   FAIL: {}",
        screens.len(),
        screens.len() as u64 - pass
    );
    println!("failure reasons: {fail:?}");
    screens.sort_by_key(|s| std::cmp::Reverse(s.occurrences));
    println!("\ntop traded mints:");
    for s in screens.iter().take(12) {
        println!("   {} n={:<3} {}", s.mint, s.occurrences, s.verdict);
    }

    // How many transactions are fully screenable (every mint passes)?
    let pass_set: BTreeSet<&String> = screens
        .iter()
        .filter(|s| s.verdict == "PASS")
        .map(|s| &s.mint)
        .collect();
    let fully_safe = recons
        .iter()
        .filter(|r| !r.token_mints.is_empty() && r.token_mints.iter().all(|m| pass_set.contains(m)))
        .count();
    println!(
        "\ntransactions whose EVERY token mint passes our screen: {fully_safe}/{}",
        recons.len()
    );
    // The intersection that actually matters for us.
    let buildable = recons
        .iter()
        .filter(|r| {
            r.hops.len() == 2
                && r.distinct_mints <= 2
                && !r.token_mints.is_empty()
                && r.token_mints.iter().all(|m| pass_set.contains(m))
        })
        .count();
    println!(
        "transactions that are BOTH 2-hop AND fully screenable: {buildable}/{}",
        recons.len()
    );

    let path = format!("{out}/recon-{}.json", now_ms());
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({
            "transactions": recons,
            "mint_screen": screens,
            "pool_candidates": pool_candidates,
        }))?,
    )?;
    println!("\nwrote {path}");
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn recon_one(rpc: &RpcClient, sig_s: &str) -> std::result::Result<ReconTx, String> {
    let sig = Signature::from_str(sig_s).map_err(|_| "bad_signature".to_string())?;
    let cfg = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::JsonParsed),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };
    let tx = rpc
        .get_transaction_with_config(&sig, cfg)
        .map_err(|e| format!("rpc:{e}"))?;
    let v = serde_json::to_value(&tx).map_err(|_| "serialize".to_string())?;
    let meta = &v["meta"];
    let msg = &v["transaction"]["message"];
    let keys: Vec<String> = msg["accountKeys"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|k| k["pubkey"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    let signer = msg["accountKeys"]
        .as_array()
        .and_then(|a| a.iter().find(|k| k["signer"].as_bool() == Some(true)))
        .and_then(|k| k["pubkey"].as_str())
        .unwrap_or_default()
        .to_string();
    let used_alt = msg["addressTableLookups"]
        .as_array()
        .map(|a| !a.is_empty())
        .unwrap_or(false);

    let mut hops: Vec<Hop> = Vec::new();
    let mut venues: BTreeSet<String> = BTreeSet::new();
    let mut custom_programs: BTreeSet<String> = BTreeSet::new();
    let mut flash_markers: BTreeSet<String> = BTreeSet::new();
    let mut pool_lookup: Vec<(usize, Vec<String>)> = Vec::new();

    let push_hop = |ix: &serde_json::Value,
                    location: String,
                    depth: u8,
                    hops: &mut Vec<Hop>,
                    venues: &mut BTreeSet<String>,
                    pool_lookup: &mut Vec<(usize, Vec<String>)>| {
        let Some(pid) = ix["programId"].as_str() else {
            return;
        };
        if let Some(v) = dex_of(pid) {
            venues.insert(v.to_string());
            let accs: Vec<String> = ix["accounts"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| x.as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();
            pool_lookup.push((hops.len(), accs.clone()));
            hops.push(Hop {
                venue: v.to_string(),
                location,
                depth,
                pool: None,
                n_accounts: accs.len(),
            });
        }
    };

    if let Some(outer) = msg["instructions"].as_array() {
        for (i, ix) in outer.iter().enumerate() {
            if let Some(pid) = ix["programId"].as_str() {
                if let Some(f) = flash_of(pid) {
                    flash_markers.insert(f.to_string());
                }
                // A top-level program that is neither a DEX nor a system-ish
                // program is very likely the operator's own arb executor.
                if dex_of(pid).is_none()
                    && !pid.starts_with("11111111")
                    && !pid.starts_with("ComputeBudget")
                    && !pid.starts_with("Token")
                    && !pid.starts_with("ATokenGPv")
                {
                    custom_programs.insert(pid.to_string());
                }
            }
            push_hop(
                ix,
                format!("outer:{i}"),
                1,
                &mut hops,
                &mut venues,
                &mut pool_lookup,
            );
        }
    }
    if let Some(inner) = meta["innerInstructions"].as_array() {
        for grp in inner {
            let oi = grp["index"].as_u64().unwrap_or(0);
            if let Some(ixs) = grp["instructions"].as_array() {
                for (j, ix) in ixs.iter().enumerate() {
                    if let Some(pid) = ix["programId"].as_str() {
                        if let Some(f) = flash_of(pid) {
                            flash_markers.insert(f.to_string());
                        }
                    }
                    let depth = ix["stackHeight"].as_u64().unwrap_or(2) as u8;
                    push_hop(
                        ix,
                        format!("inner:{oi}.{j}"),
                        depth,
                        &mut hops,
                        &mut venues,
                        &mut pool_lookup,
                    );
                }
            }
        }
    }

    // Resolve which account in each hop is the pool (owned by the DEX program).
    let mut all: BTreeSet<String> = BTreeSet::new();
    for (_, accs) in &pool_lookup {
        for a in accs.iter().take(12) {
            all.insert(a.clone());
        }
    }
    let list: Vec<Pubkey> = all
        .iter()
        .filter_map(|a| Pubkey::from_str(a).ok())
        .collect();
    let mut owner_of: HashMap<String, String> = HashMap::new();
    for chunk in list.chunks(100) {
        if let Ok(accs) = rpc.get_multiple_accounts(chunk) {
            for (k, acc) in chunk.iter().zip(accs) {
                if let Some(a) = acc {
                    owner_of.insert(k.to_string(), a.owner.to_string());
                }
            }
        }
    }
    for (idx, accs) in &pool_lookup {
        let venue_prog = DEX_PROGRAMS
            .iter()
            .find(|(_, v)| *v == hops[*idx].venue)
            .map(|(p, _)| *p);
        if let Some(vp) = venue_prog {
            hops[*idx].pool = accs
                .iter()
                .find(|a| owner_of.get(*a).map(|o| o == vp).unwrap_or(false))
                .cloned();
        }
    }

    // Token mints that actually moved (from token balances), excluding WSOL.
    let mut mints: BTreeSet<String> = BTreeSet::new();
    for side in ["preTokenBalances", "postTokenBalances"] {
        if let Some(arr) = meta[side].as_array() {
            for b in arr {
                if let Some(m) = b["mint"].as_str() {
                    if m != WSOL {
                        mints.insert(m.to_string());
                    }
                }
            }
        }
    }

    let top_level_hops = hops.iter().filter(|h| h.depth <= 1).count();
    let nested_hops = hops.len() - top_level_hops;
    let net_profit = 0; // filled by the caller's dataset; not recomputed here

    Ok(ReconTx {
        sig: sig_s.to_string(),
        slot: v["slot"].as_u64().unwrap_or(0),
        block_time: v["blockTime"].as_i64(),
        signer,
        venues: venues.into_iter().collect(),
        distinct_mints: mints.len(),
        token_mints: mints.into_iter().collect(),
        top_level_hops,
        nested_hops,
        hops,
        custom_programs: custom_programs.into_iter().collect(),
        flash_markers: flash_markers.into_iter().collect(),
        net_profit,
        compute_units: meta["computeUnitsConsumed"].as_u64(),
        n_account_keys: keys.len(),
        used_alt,
    })
}
