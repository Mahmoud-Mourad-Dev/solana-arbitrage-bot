//! S15A / Phase 0.5 — READ-ONLY forensic scan of landed multi-DEX transactions.
//!
//! Purpose: decide, from realized on-chain profit, whether a route family is
//! worth implementing — BEFORE writing any new venue code. Answers three
//! questions with numbers:
//!   1. Does this family produce REPEATED realized net profit?
//!   2. Who wins it (signer concentration, tip share of gross)?
//!   3. Could we plausibly detect it from public RPC/Geyser?
//!
//! Evidence rules (non-negotiable):
//!   * Realized profit comes ONLY from balance deltas — never a quote, a log
//!     claim, or a program return value.
//!   * A transaction is NOT arbitrage merely because it invokes two swap
//!     programs. It must show a closed WSOL/SOL round trip with positive net.
//!   * Every rejected transaction gets a typed reason.
//!
//! NEVER builds, signs, simulates or submits anything. Read-only RPC.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status_client_types::UiTransactionEncoding;
use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;

const WSOL: &str = "So11111111111111111111111111111111111111112";

/// Known DEX / aggregator program ids → stable family label.
const KNOWN_PROGRAMS: &[(&str, &str)] = &[
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
    ("obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y", "obric"),
    ("SoLFiHG9TfgtdUXUjWAxi3LtvYuFyDLVhBWxdMZxyCe", "solfi"),
    ("ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY", "zerofi"),
    ("PSwapMdSai8tjrEXcxFeQth87xC4rRsa4VA5mhGhXkP", "penguin"),
    ("stabbG4HoDVUwLVR4ANuKUhLDXvxUsNjcYqAv6JTPnQ", "stabble"),
    (
        "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
        "pump-bonding",
    ),
];

/// Jito tip accounts (mainnet) — a transfer to any of these is a visible tip.
const JITO_TIPS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

fn family_of(program: &str) -> Option<&'static str> {
    KNOWN_PROGRAMS
        .iter()
        .find(|(p, _)| *p == program)
        .map(|(_, f)| *f)
}

// ─────────────────────────── records ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedTx {
    pub sig: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub signer: String,
    /// Sorted, de-duplicated DEX families actually invoked.
    pub dex_families: Vec<String>,
    /// Structural family key, e.g. "raydium-clmm+raydium-v4".
    pub route_family: String,
    pub classification: String,
    /// Realized signer SOL delta (lamports, includes fees paid).
    pub sol_delta: i128,
    /// Realized signer WSOL token-account delta (lamports).
    pub wsol_delta: i128,
    /// Transaction fee actually charged (lamports).
    pub fee: u64,
    /// Visible Jito tip transfers (lamports).
    pub jito_tip: u64,
    /// gross = wsol_delta + sol_delta + fee + tip (what the trade earned before
    /// paying network + tip).
    pub gross_profit: i128,
    /// net = gross - fee - tip (what the operator actually kept).
    pub net_profit: i128,
    pub compute_units: Option<u64>,
    pub n_dex_instructions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScanSummary {
    pub scanned: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub reject_reasons: BTreeMap<String, u64>,
    pub family_counts: BTreeMap<String, u64>,
    pub family_net_profit: BTreeMap<String, i128>,
}

/// Typed rejection reasons — every scanned tx that is not accepted gets one.
pub fn reject_reason(code: &str) -> String {
    code.to_string()
}

/// Per-family persistence accumulator. `hours` is the set of distinct
/// wall-clock hours in which the family produced profitable arbitrage — the
/// metric that separates a repeated business from a few lucky events.
#[derive(Debug, Default, Clone)]
pub struct PersistenceStat {
    pub n: u64,
    pub total_net: i128,
    pub nets: Vec<i128>,
    pub signers: HashSet<String>,
    pub hours: HashSet<i64>,
    pub min_time: Option<i64>,
    pub max_time: Option<i64>,
}

/// Classify by structure. A tx is only `PURE_ATOMIC_ARBITRAGE` when it closes a
/// WSOL round trip with positive net inside ONE transaction and touches ≥2 DEX
/// programs. Everything else is labelled honestly.
pub fn classify(dex_families: &[String], net: i128, closed_loop: bool) -> &'static str {
    if dex_families.len() < 2 {
        return "UNCLEAR_REJECTED";
    }
    if !closed_loop {
        return "OTHER_MEV";
    }
    if net > 0 {
        "PURE_ATOMIC_ARBITRAGE"
    } else {
        "OTHER_MEV"
    }
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    let args: Vec<String> = std::env::args().collect();

    // Seed accounts to sample signatures from (busy pools / programs).
    let seeds: Vec<String> = args
        .iter()
        .position(|a| a == "--seeds")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_else(|| {
            vec![
                // Raydium CLMM SOL/USDC (busy)
                "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv".into(),
                // Raydium AMM v4 SOL/USDC (busy)
                "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2".into(),
            ]
        });
    let per_seed: usize = args
        .iter()
        .position(|a| a == "--limit")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let out = std::env::var("FORENSIC_OUT").unwrap_or_else(|_| "reports/forensic".into());
    std::fs::create_dir_all(&out).ok();

    println!("=== S15A / Phase 0.5 forensic scan (READ-ONLY) ===");
    println!("seeds: {} | per-seed signatures: {per_seed}", seeds.len());

    // Signature collection with PAGINATION (`before` cursor) so a seed can
    // contribute more than one 1000-signature page and the sample spans a
    // longer wall-clock window — required for the persistence measurement.
    let mut sigs: Vec<(String, u64)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for s in &seeds {
        let Ok(key) = Pubkey::from_str(s) else {
            println!("  seed {s}: INVALID pubkey — skipped");
            continue;
        };
        let mut n = 0usize;
        let mut before: Option<Signature> = None;
        while n < per_seed {
            let cfg = solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                before,
                until: None,
                limit: Some(1000.min(per_seed - n).max(1)),
                commitment: Some(CommitmentConfig::confirmed()),
            };
            let list = match rpc.get_signatures_for_address_with_config(&key, cfg) {
                Ok(l) => l,
                Err(e) => {
                    println!(
                        "  seed {}… page failed ({e}) — keeping what we have",
                        &s[..8]
                    );
                    break;
                }
            };
            if list.is_empty() {
                break;
            }
            let last = list
                .last()
                .and_then(|x| Signature::from_str(&x.signature).ok());
            for si in list.into_iter().filter(|x| x.err.is_none()) {
                if n >= per_seed {
                    break;
                }
                if seen.insert(si.signature.clone()) {
                    sigs.push((si.signature, si.slot));
                    n += 1;
                }
            }
            match last {
                Some(l) => before = Some(l),
                None => break,
            }
        }
        println!("  seed {}… collected {n}", &s[..8]);
    }
    println!("total unique successful signatures: {}", sigs.len());

    let mut summary = ScanSummary::default();
    let mut accepted: Vec<AcceptedTx> = Vec::new();
    let mut rejected_log: Vec<(String, String)> = Vec::new();

    for (sig_s, _slot) in &sigs {
        summary.scanned += 1;
        match analyze(&rpc, sig_s) {
            Ok(Some(a)) => {
                summary.accepted += 1;
                *summary
                    .family_counts
                    .entry(a.route_family.clone())
                    .or_default() += 1;
                *summary
                    .family_net_profit
                    .entry(a.route_family.clone())
                    .or_default() += a.net_profit;
                accepted.push(a);
            }
            Ok(None) => {}
            Err(reason) => {
                summary.rejected += 1;
                *summary.reject_reasons.entry(reason.clone()).or_default() += 1;
                rejected_log.push((sig_s.clone(), reason));
            }
        }
    }

    // ── PERSISTENCE: is a family a repeated business, or a few lucky events? ──
    // Only PURE_ATOMIC_ARBITRAGE counts. For each family we report the number of
    // distinct hours it produced profit in, its span, and how concentrated the
    // profit is in its single best transaction.
    let arbs: Vec<&AcceptedTx> = accepted
        .iter()
        .filter(|a| a.classification == "PURE_ATOMIC_ARBITRAGE")
        .collect();
    let mut fam_stats: BTreeMap<String, PersistenceStat> = BTreeMap::new();
    for a in &arbs {
        let e = fam_stats.entry(a.route_family.clone()).or_default();
        e.n += 1;
        e.total_net += a.net_profit;
        e.nets.push(a.net_profit);
        e.signers.insert(a.signer.clone());
        if let Some(t) = a.block_time {
            e.hours.insert(t / 3600);
            e.min_time = e.min_time.map_or(Some(t), |m: i64| Some(m.min(t)));
            e.max_time = e.max_time.map_or(Some(t), |m: i64| Some(m.max(t)));
        }
    }
    println!(
        "\n═══ PERSISTENCE (PURE_ATOMIC_ARBITRAGE only, n={}) ═══",
        arbs.len()
    );
    println!(
        "{:<46}{:>4}{:>14}{:>12}{:>8}{:>8}{:>9}{:>9}",
        "family", "n", "total_SOL", "median", "hours", "span_h", "signers", "top_share"
    );
    let mut rows: Vec<(&String, &PersistenceStat)> = fam_stats.iter().collect();
    rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.total_net));
    for (fam, s) in &rows {
        let mut nets = s.nets.clone();
        nets.sort_unstable();
        let median = nets[nets.len() / 2];
        let span_h = match (s.min_time, s.max_time) {
            (Some(a), Some(b)) => (b - a) as f64 / 3600.0,
            _ => 0.0,
        };
        // Concentration: share of the family's total net held by its best tx.
        let top = nets.last().copied().unwrap_or(0);
        let share = if s.total_net > 0 {
            top as f64 * 100.0 / s.total_net as f64
        } else {
            0.0
        };
        println!(
            "{:<46}{:>4}{:>14.6}{:>12}{:>8}{:>8.1}{:>9}{:>8.0}%",
            fam.chars().take(45).collect::<String>(),
            s.n,
            s.total_net as f64 / 1e9,
            median,
            s.hours.len(),
            span_h,
            s.signers.len(),
            share
        );
    }

    // ── report ──
    println!(
        "\nscanned={} accepted={} rejected={}",
        summary.scanned, summary.accepted, summary.rejected
    );
    println!("reject reasons: {:?}", summary.reject_reasons);
    println!("\nroute families (accepted, by count):");
    let mut fams: Vec<(&String, &u64)> = summary.family_counts.iter().collect();
    fams.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    for (f, c) in &fams {
        let net = summary.family_net_profit.get(*f).copied().unwrap_or(0);
        println!(
            "  {f:<40} n={c:<4} total_net={net} lamports ({:.6} SOL)",
            net as f64 / 1e9
        );
    }

    // Signer concentration + tip share (the "who wins" question).
    let mut by_signer: BTreeMap<String, (u64, i128)> = BTreeMap::new();
    let (mut tip_total, mut gross_total) = (0i128, 0i128);
    for a in &accepted {
        let e = by_signer.entry(a.signer.clone()).or_default();
        e.0 += 1;
        e.1 += a.net_profit;
        tip_total += a.jito_tip as i128;
        gross_total += a.gross_profit.max(0);
    }
    let mut signers: Vec<(&String, &(u64, i128))> = by_signer.iter().collect();
    signers.sort_by_key(|(_, v)| std::cmp::Reverse(v.0));
    println!("\ntop signers (accepted txs):");
    for (s, (n, net)) in signers.iter().take(10) {
        println!("  {}… n={n} net={net}", &s[..8.min(s.len())]);
    }
    if gross_total > 0 {
        println!(
            "\nvisible Jito tip share of gross: {:.1}%  (tips {} / gross {})",
            tip_total as f64 * 100.0 / gross_total as f64,
            tip_total,
            gross_total
        );
    }

    let path = format!("{out}/forensic-{}.json", now_ms());
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({
            "summary": summary,
            "accepted": accepted,
            "rejected_sample": rejected_log.iter().take(50).collect::<Vec<_>>(),
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

/// Analyze one signature. `Ok(Some)` = accepted arbitrage-shaped tx,
/// `Ok(None)` = not multi-DEX (silently skipped), `Err(reason)` = typed reject.
fn analyze(rpc: &RpcClient, sig_s: &str) -> std::result::Result<Option<AcceptedTx>, String> {
    let sig = Signature::from_str(sig_s).map_err(|_| "bad_signature".to_string())?;
    let cfg = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::JsonParsed),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };
    let tx = rpc
        .get_transaction_with_config(&sig, cfg)
        .map_err(|_| "rpc_fetch_failed".to_string())?;
    let v = serde_json::to_value(&tx).map_err(|_| "serialize_failed".to_string())?;
    let meta = &v["meta"];
    if !meta["err"].is_null() {
        return Err("tx_failed".into());
    }
    let msg = &v["transaction"]["message"];
    let keys: Vec<String> = msg["accountKeys"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|k| k["pubkey"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    if keys.is_empty() {
        return Err("no_account_keys".into());
    }
    let signer = msg["accountKeys"]
        .as_array()
        .and_then(|a| a.iter().find(|k| k["signer"].as_bool() == Some(true)))
        .and_then(|k| k["pubkey"].as_str())
        .unwrap_or(&keys[0])
        .to_string();

    // Collect DEX programs from outer + inner instructions.
    let mut families: HashSet<String> = HashSet::new();
    let mut n_dex_ix = 0usize;
    let mut scan_ix = |ix: &serde_json::Value| {
        if let Some(p) = ix["programId"].as_str() {
            if let Some(f) = family_of(p) {
                families.insert(f.to_string());
                n_dex_ix += 1;
            }
        }
    };
    if let Some(outer) = msg["instructions"].as_array() {
        for ix in outer {
            scan_ix(ix);
        }
    }
    if let Some(inner) = meta["innerInstructions"].as_array() {
        for grp in inner {
            if let Some(ixs) = grp["instructions"].as_array() {
                for ix in ixs {
                    scan_ix(ix);
                }
            }
        }
    }
    // Aggregator-only routing is not a DEX-pair family we can implement.
    let dex_only: Vec<String> = families
        .iter()
        .filter(|f| f.as_str() != "jupiter-v6")
        .cloned()
        .collect();
    if dex_only.len() < 2 {
        return Ok(None); // not a multi-DEX candidate — silently skipped
    }

    // ── realized balances (the ONLY profit source) ──
    let pre_bal = meta["preBalances"].as_array().ok_or("no_pre_balances")?;
    let post_bal = meta["postBalances"].as_array().ok_or("no_post_balances")?;
    let signer_idx = keys.iter().position(|k| *k == signer).unwrap_or(0);
    let sol_delta = post_bal
        .get(signer_idx)
        .and_then(|x| x.as_i64())
        .unwrap_or(0) as i128
        - pre_bal
            .get(signer_idx)
            .and_then(|x| x.as_i64())
            .unwrap_or(0) as i128;
    let fee = meta["fee"].as_u64().unwrap_or(0);

    // Signer's WSOL token-account delta.
    let tok_delta = |side: &str| -> i128 {
        meta[side]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|b| {
                        b["mint"].as_str() == Some(WSOL) && b["owner"].as_str() == Some(&signer)
                    })
                    .filter_map(|b| b["uiTokenAmount"]["amount"].as_str())
                    .filter_map(|s| s.parse::<i128>().ok())
                    .sum()
            })
            .unwrap_or(0)
    };
    let wsol_delta = tok_delta("postTokenBalances") - tok_delta("preTokenBalances");

    // Visible Jito tips: SOL increase on a known tip account.
    let mut jito_tip: u64 = 0;
    for (i, k) in keys.iter().enumerate() {
        if JITO_TIPS.contains(&k.as_str()) {
            let d = post_bal.get(i).and_then(|x| x.as_i64()).unwrap_or(0)
                - pre_bal.get(i).and_then(|x| x.as_i64()).unwrap_or(0);
            if d > 0 {
                jito_tip += d as u64;
            }
        }
    }

    // net = what the operator kept (SOL + WSOL movement, fee already inside
    // sol_delta, tip already inside sol_delta as an outflow).
    let net_profit = sol_delta + wsol_delta;
    // gross = net before network fee and tip.
    let gross_profit = net_profit + fee as i128 + jito_tip as i128;

    // Closed loop = the signer ends up with more of the base asset than they
    // started, i.e. the trade actually round-tripped rather than swapping out.
    let closed_loop = wsol_delta != 0 || sol_delta != -(fee as i128);
    let mut fams_sorted = dex_only.clone();
    fams_sorted.sort();
    let route_family = fams_sorted.join("+");
    let classification = classify(&fams_sorted, net_profit, closed_loop).to_string();

    Ok(Some(AcceptedTx {
        sig: sig_s.to_string(),
        slot: v["slot"].as_u64().unwrap_or(0),
        block_time: v["blockTime"].as_i64(),
        signer,
        dex_families: fams_sorted,
        route_family,
        classification,
        sol_delta,
        wsol_delta,
        fee,
        jito_tip,
        gross_profit,
        net_profit,
        compute_units: meta["computeUnitsConsumed"].as_u64(),
        n_dex_instructions: n_dex_ix,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_lookup_is_exact() {
        assert_eq!(
            family_of("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
            Some("raydium-clmm")
        );
        assert_eq!(
            family_of("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"),
            Some("raydium-v4")
        );
        assert_eq!(family_of("NotAProgram"), None);
    }

    #[test]
    fn classification_never_calls_single_dex_arbitrage() {
        // One swap program is never arbitrage, no matter how profitable.
        assert_eq!(
            classify(&["raydium-v4".into()], 10_000_000, true),
            "UNCLEAR_REJECTED"
        );
    }

    #[test]
    fn classification_requires_closed_loop_and_positive_net() {
        let fams = vec!["raydium-clmm".to_string(), "raydium-v4".to_string()];
        assert_eq!(classify(&fams, 5_000, true), "PURE_ATOMIC_ARBITRAGE");
        // Multi-DEX but not a closed round trip → not arbitrage.
        assert_eq!(classify(&fams, 5_000, false), "OTHER_MEV");
        // Closed loop but negative net → not counted as profitable arbitrage.
        assert_eq!(classify(&fams, -5_000, true), "OTHER_MEV");
    }
}
