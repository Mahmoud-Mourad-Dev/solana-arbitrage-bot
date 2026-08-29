//! The four-question forensic pipeline over one venue pair, generalized from
//! `monitor/src/bin/forensics_s15b.rs` (S15B).
//!
//! Q1 reachability · Q2 leader independence · Q3 land rate · Q4 economics.
//! READ-ONLY: every RPC call here is a read. No transaction is constructed.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use solana_client::rpc_client::{GetConfirmedSignaturesForAddress2Config, RpcClient};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use super::price::{local_price, median_price, value_pnl, PricePoint};
use super::schema::{EvidenceTx, InputV2, WSOL_MINT};

/// Base fee for a 1-signature transaction — the economic floor an opportunity
/// must clear before it is bankable at all.
pub const SIG_FEE_FLOOR_LAMPORTS: i128 = 5_000;

/// Fixed thresholds for the Q4 distribution table (lamports, net).
pub const Q4_THRESHOLDS: [i128; 7] = [0, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000];

/// The minimum number of clean two-sided price points required before
/// quote-priced P&L is computed at all. Below this, only inventory-neutral
/// cycles are counted and everything else is `Unpriceable` — never estimated.
pub const MIN_PRICE_POINTS: usize = 50;

/// Local price window (nearest K points by slot), as used in S15B.
pub const PRICE_WINDOW_K: usize = 151;

// ─────────────────────────── RPC retry ───────────────────────────

/// Run a fallible RPC closure with exponential backoff. Retries transport
/// errors and 429s; gives up after `tries` and returns the last error.
fn with_retry<T>(tries: u32, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut delay_ms = 400u64;
    let mut last: Option<anyhow::Error> = None;
    for _ in 0..tries {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 9 / 5).min(20_000);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("retry: no attempt ran")))
}

// ─────────────────────────── census (Q3 population) ───────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CensusTx {
    pub sig: String,
    pub slot: u64,
    pub err: bool,
    /// Which of the input's pools this tx touched (sorted, deduped).
    pub pools: Vec<String>,
    pub touched_a: bool,
    pub touched_b: bool,
}

#[derive(Debug, Serialize)]
pub struct Census {
    /// Distinct txs touching any input pool inside the window.
    pub total_txs: usize,
    /// Txs touching ≥1 pool on each side (the cross-venue set).
    pub cross: Vec<CensusTx>,
    pub per_pool_counts: BTreeMap<String, usize>,
}

/// Enumerate all signatures for `pool` inside `[slot_min, slot_max]`.
///
/// FAILS LOUDLY on truncation: if `max_pages` is exhausted before the walk
/// reaches `slot_min`, this is an error, not a smaller answer. A silent
/// pagination truncation is one of the three measurement errors S15B caught
/// in itself.
pub fn enumerate_pool_sigs(
    rpc: &RpcClient,
    pool: &str,
    slot_min: u64,
    slot_max: u64,
    max_pages: usize,
) -> Result<Vec<(String, u64, bool)>> {
    let pk = Pubkey::from_str(pool).with_context(|| format!("bad pool address {pool}"))?;
    let mut out = Vec::new();
    let mut before: Option<Signature> = None;
    for _page in 0..max_pages {
        let sigs = with_retry(8, || {
            let cfg = GetConfirmedSignaturesForAddress2Config {
                before,
                until: None,
                limit: Some(1000),
                commitment: Some(CommitmentConfig::confirmed()),
            };
            rpc.get_signatures_for_address_with_config(&pk, cfg)
                .map_err(|e| anyhow::anyhow!("getSignaturesForAddress {pool}: {e}"))
        })?;
        let n = sigs.len();
        for s in &sigs {
            if s.slot > slot_max {
                continue;
            }
            if s.slot < slot_min {
                return finish_page(out, sigs, slot_min);
            }
            out.push((s.signature.clone(), s.slot, s.err.is_some()));
        }
        if n < 1000 {
            // History exhausted before slot_min: the pool simply has no older
            // txs. That is a complete answer, not a truncation.
            return Ok(out);
        }
        before = Signature::from_str(&sigs[n - 1].signature).ok();
    }
    bail!(
        "TRUNCATED: pool {pool} pagination hit the {max_pages}-page cap before \
         reaching slot {slot_min}. Refusing to return a partial census — raise \
         --max-pages or shrink the window."
    )
}

fn finish_page(
    out: Vec<(String, u64, bool)>,
    _sigs: Vec<solana_client::rpc_response::RpcConfirmedTransactionStatusWithSignature>,
    _slot_min: u64,
) -> Result<Vec<(String, u64, bool)>> {
    Ok(out)
}

/// Build the census for an input: every tx touching any pool, and the
/// cross-venue subset.
pub fn census(rpc: &RpcClient, input: &InputV2, max_pages: usize) -> Result<Census> {
    let (side_a, side_b) = input.side_pools();
    let a_set: BTreeSet<&String> = side_a.iter().collect();
    let mut per_pool_counts = BTreeMap::new();
    // sig -> (slot, err, pools)
    let mut map: BTreeMap<String, (u64, bool, BTreeSet<String>)> = BTreeMap::new();
    for pool in side_a.iter().chain(side_b.iter()) {
        let sigs = enumerate_pool_sigs(rpc, pool, input.slot_min, input.slot_max, max_pages)?;
        per_pool_counts.insert(pool.clone(), sigs.len());
        for (sig, slot, err) in sigs {
            let e = map
                .entry(sig)
                .or_insert_with(|| (slot, err, BTreeSet::new()));
            e.2.insert(pool.clone());
        }
    }
    let total_txs = map.len();
    let mut cross: Vec<CensusTx> = map
        .into_iter()
        .filter_map(|(sig, (slot, err, pools))| {
            let touched_a = pools.iter().any(|p| a_set.contains(p));
            let touched_b = pools.iter().any(|p| !a_set.contains(p));
            (touched_a && touched_b).then(|| CensusTx {
                sig,
                slot,
                err,
                pools: pools.into_iter().collect(),
                touched_a,
                touched_b,
            })
        })
        .collect();
    cross.sort_by_key(|t| t.slot);
    Ok(Census {
        total_txs,
        cross,
        per_pool_counts,
    })
}

// ─────────────────────────── per-tx accounting ───────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TxAccounting {
    pub sig: String,
    pub slot: u64,
    pub signer: String,
    pub fee: u64,
    /// Native SOL delta on the signer's system account (post - pre). Includes
    /// the fee, so downstream P&L is already net of it.
    pub d_sol: i128,
    /// Native SOL contributed to this transaction by accounts that are NOT
    /// the signer and NOT token accounts — the operator topping up trade
    /// capital from a co-owned wallet or a program-owned vault mid-tx.
    ///
    /// Booking these inflows as profit is the S15A accounting error in a
    /// subtler form: on the sampled top-85 events of the first batch run,
    /// 11/85 were PURE capital transfers (strict value = exactly -fee) and
    /// the aggregate top-end overstatement was 14.9%. Arb revenue reaches the
    /// signer exclusively through pool-vault token outflows, so external
    /// native inflows are excluded from value P&L.
    pub external_native_inflow: i128,
    /// Signer-owned SPL balance deltas by mint (post - pre), token units.
    pub deltas: BTreeMap<String, i128>,
}

/// Extract accounting facts from a jsonParsed `getTransaction` result.
/// Pure over the parsed JSON — unit-tested against a literal fixture.
pub fn account_tx(sig: &str, v: &serde_json::Value) -> Option<TxAccounting> {
    let meta = &v["meta"];
    let msg = &v["transaction"]["message"];
    let keys = msg["accountKeys"].as_array()?;
    let (signer_idx, signer) = keys.iter().enumerate().find_map(|(i, k)| {
        (k["signer"].as_bool() == Some(true)).then(|| (i, k["pubkey"].as_str().unwrap_or("")))
    })?;
    let pre = meta["preBalances"].as_array()?;
    let post = meta["postBalances"].as_array()?;
    let d_sol = post.get(signer_idx)?.as_i64()? as i128 - pre.get(signer_idx)?.as_i64()? as i128;
    // Indices that carry SPL token balances (vaults, ATAs, fee collectors) —
    // their native movements are rent/wrapping mechanics, not capital inflows.
    let mut token_idx: BTreeSet<usize> = BTreeSet::new();
    for arr in [
        meta["preTokenBalances"].as_array(),
        meta["postTokenBalances"].as_array(),
    ] {
        for b in arr.unwrap_or(&Vec::new()) {
            if let Some(i) = b["accountIndex"].as_u64() {
                token_idx.insert(i as usize);
            }
        }
    }
    let mut external_native_inflow: i128 = 0;
    for (j, (a, b)) in pre.iter().zip(post.iter()).enumerate() {
        if j == signer_idx || token_idx.contains(&j) {
            continue;
        }
        let (a, b) = (a.as_i64()? as i128, b.as_i64()? as i128);
        if a > b {
            external_native_inflow += a - b;
        }
    }
    let mut deltas: BTreeMap<String, i128> = BTreeMap::new();
    for (arr, sign) in [
        (meta["preTokenBalances"].as_array(), -1i128),
        (meta["postTokenBalances"].as_array(), 1i128),
    ] {
        for b in arr.unwrap_or(&Vec::new()) {
            if b["owner"].as_str() != Some(signer) {
                continue;
            }
            let mint = b["mint"].as_str()?.to_string();
            let amt: i128 = b["uiTokenAmount"]["amount"].as_str()?.parse().ok()?;
            *deltas.entry(mint).or_default() += sign * amt;
        }
    }
    deltas.retain(|_, d| *d != 0);
    Some(TxAccounting {
        sig: sig.to_string(),
        slot: v["slot"].as_u64().unwrap_or(0),
        signer: signer.to_string(),
        fee: meta["fee"].as_u64().unwrap_or(0),
        d_sol,
        external_native_inflow,
        deltas,
    })
}

/// Fetch and account one transaction (jsonParsed, v0-capable).
pub fn fetch_accounting(rpc: &RpcClient, sig: &str, slot_hint: u64) -> Result<TxAccounting> {
    let raw = with_retry(8, || {
        let req = serde_json::json!([
            sig,
            {"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0,
             "commitment": "confirmed"}
        ]);
        rpc.send::<serde_json::Value>(solana_client::rpc_request::RpcRequest::GetTransaction, req)
            .map_err(|e| anyhow::anyhow!("getTransaction {sig}: {e}"))
    })?;
    let mut acct = account_tx(sig, &raw).with_context(|| format!("unaccountable tx {sig}"))?;
    if acct.slot == 0 {
        acct.slot = slot_hint;
    }
    Ok(acct)
}

// ─────────────────────────── value classification ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Pnl {
    /// SOL-equivalent value in lamports, net of the fee the signer paid.
    Priced(i128),
    /// Carries balance changes this pipeline cannot price honestly
    /// (non-quote token deltas, or quote pricing unavailable). Never estimated.
    Unpriceable,
}

/// Classify one accounted tx. `price` is the LOCAL market price of the quote
/// mint (None ⇒ quote pricing unavailable).
pub fn classify_value(acct: &TxAccounting, quote_mint: &str, price: Option<&PricePoint>) -> Pnl {
    let d_wsol = acct.deltas.get(WSOL_MINT).copied().unwrap_or(0);
    let d_quote = if quote_mint == WSOL_MINT {
        0
    } else {
        acct.deltas.get(quote_mint).copied().unwrap_or(0)
    };
    let has_other = acct
        .deltas
        .iter()
        .any(|(m, d)| m != WSOL_MINT && m != quote_mint && *d != 0);
    if has_other {
        return Pnl::Unpriceable;
    }
    // STRICT: capital moved in from the operator's other accounts is not
    // revenue. Subtracting it makes a pure consolidation transfer read as
    // exactly -fee instead of a fake win (see external_native_inflow docs).
    let d_sol = acct.d_sol - acct.external_native_inflow;
    if quote_mint == WSOL_MINT || d_quote == 0 {
        // Inventory-neutral cycle: value is exactly the SOL delta.
        return Pnl::Priced(d_sol + d_wsol);
    }
    match price {
        Some(p) => Pnl::Priced(value_pnl(d_sol, d_wsol, d_quote, p)),
        None => Pnl::Unpriceable,
    }
}

/// Clean two-sided price points from the accounted population: only WSOL and
/// quote move, in opposite directions.
pub fn extract_price_points(accts: &[TxAccounting], quote_mint: &str) -> Vec<PricePoint> {
    if quote_mint == WSOL_MINT {
        return Vec::new();
    }
    let mut pts: Vec<PricePoint> = accts
        .iter()
        .filter_map(|a| {
            let d_wsol = a.deltas.get(WSOL_MINT).copied().unwrap_or(0);
            let d_quote = a.deltas.get(quote_mint).copied().unwrap_or(0);
            let sol = a.d_sol + d_wsol;
            let other = a
                .deltas
                .iter()
                .any(|(m, d)| m != WSOL_MINT && m != quote_mint && *d != 0);
            if other || d_quote == 0 || sol == 0 || (sol > 0) == (d_quote > 0) {
                return None;
            }
            Some(PricePoint {
                slot: a.slot,
                quote_units: d_quote.unsigned_abs(),
                lamports: sol.unsigned_abs(),
            })
        })
        .collect();
    pts.sort_by_key(|p| p.slot);
    pts
}

// ─────────────────────────── Q4 aggregation ───────────────────────────

#[derive(Debug, Serialize)]
pub struct ThresholdRow {
    pub threshold_lamports: i128,
    pub events: usize,
    pub events_per_day: f64,
    pub sum_lamports: i128,
    pub distinct_signers: usize,
}

#[derive(Debug, Serialize)]
pub struct Q4Report {
    pub landed_cross: usize,
    pub failed_cross: usize,
    pub accounted: usize,
    pub unpriceable: usize,
    pub priced: usize,
    pub price_points: usize,
    /// "quote-priced" | "cycles-only" (quote==WSOL or too few price points)
    pub pricing_mode: String,
    pub value_positive: usize,
    pub median_positive_lamports: i128,
    pub p90_positive_lamports: i128,
    pub max_positive_lamports: i128,
    pub below_sig_fee_floor: usize,
    /// Share of total positive value captured by the top 1% / 5% / 10% of events (permille).
    pub concentration_permille: [u32; 3],
    pub thresholds: Vec<ThresholdRow>,
    pub distinct_positive_signers: usize,
    /// Top-1 signer's share of total positive value, permille.
    pub top1_signer_share_permille: u32,
    /// Median in-window quote price (units per 1e9 lamports), if derived.
    pub window_price: Option<PricePoint>,
    /// USD per SOL — only when the quote mint is a USD stable (USDC/USDT).
    pub usd_per_sol: Option<f64>,
}

pub fn q4_aggregate(
    census: &Census,
    accts: &[TxAccounting],
    pnls: &[(usize, Pnl)], // index into accts
    quote_mint: &str,
    window_hours: f64,
    price_points: &[PricePoint],
) -> Q4Report {
    let landed_cross = census.cross.iter().filter(|t| !t.err).count();
    let failed_cross = census.cross.len() - landed_cross;

    let mut positive: Vec<(i128, &str)> = Vec::new();
    let mut unpriceable = 0usize;
    let mut priced = 0usize;
    for (i, p) in pnls {
        match p {
            Pnl::Unpriceable => unpriceable += 1,
            Pnl::Priced(v) => {
                priced += 1;
                if *v > 0 {
                    positive.push((*v, accts[*i].signer.as_str()));
                }
            }
        }
    }
    positive.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
    let total_pos: i128 = positive.iter().map(|(v, _)| v).sum();
    let n_pos = positive.len();

    let pct_share = |frac_num: usize, frac_den: usize| -> u32 {
        if n_pos == 0 || total_pos == 0 {
            return 0;
        }
        let k = (n_pos * frac_num / frac_den).max(1);
        let s: i128 = positive[..k.min(n_pos)].iter().map(|(v, _)| v).sum();
        (s * 1000 / total_pos) as u32
    };

    let mut by_signer: BTreeMap<&str, i128> = BTreeMap::new();
    for (v, s) in &positive {
        *by_signer.entry(s).or_default() += v;
    }
    let top1_signer = by_signer.values().max().copied().unwrap_or(0);

    let mut sorted_vals: Vec<i128> = positive.iter().map(|(v, _)| *v).collect();
    sorted_vals.sort_unstable();

    let thresholds = Q4_THRESHOLDS
        .iter()
        .map(|&th| {
            let sel: Vec<&(i128, &str)> = positive.iter().filter(|(v, _)| *v >= th).collect();
            let sum: i128 = sel.iter().map(|(v, _)| v).sum();
            let signers: BTreeSet<&str> = sel.iter().map(|(_, s)| *s).collect();
            ThresholdRow {
                threshold_lamports: th,
                events: sel.len(),
                events_per_day: sel.len() as f64 / window_hours * 24.0,
                sum_lamports: sum,
                distinct_signers: signers.len(),
            }
        })
        .collect();

    let window_price = median_price(price_points);
    let usd_per_sol = usd_per_sol(quote_mint, window_price.as_ref());
    let pricing_mode = if quote_mint == WSOL_MINT {
        "cycles-only (quote is WSOL)".to_string()
    } else if price_points.len() < MIN_PRICE_POINTS {
        format!(
            "cycles-only ({} price points < {MIN_PRICE_POINTS} minimum)",
            price_points.len()
        )
    } else {
        "quote-priced".to_string()
    };

    Q4Report {
        landed_cross,
        failed_cross,
        accounted: accts.len(),
        unpriceable,
        priced,
        price_points: price_points.len(),
        pricing_mode,
        value_positive: n_pos,
        median_positive_lamports: sorted_vals.get(n_pos / 2).copied().unwrap_or(0),
        p90_positive_lamports: sorted_vals
            .get((n_pos * 9 / 10).min(n_pos.saturating_sub(1)))
            .copied()
            .unwrap_or(0),
        max_positive_lamports: sorted_vals.last().copied().unwrap_or(0),
        below_sig_fee_floor: sorted_vals
            .iter()
            .filter(|v| **v < SIG_FEE_FLOOR_LAMPORTS)
            .count(),
        concentration_permille: [pct_share(1, 100), pct_share(5, 100), pct_share(10, 100)],
        thresholds,
        distinct_positive_signers: by_signer.len(),
        top1_signer_share_permille: if total_pos > 0 {
            (top1_signer * 1000 / total_pos) as u32
        } else {
            0
        },
        window_price,
        usd_per_sol,
    }
}

/// USD/SOL is only honest when the quote mint IS a USD stable; anything else
/// returns None (`Unsupported` at the report layer).
pub fn usd_per_sol(quote_mint: &str, window_price: Option<&PricePoint>) -> Option<f64> {
    let is_stable = crate::mint_safety::major_asset(quote_mint).is_some(); // USDC/USDT only
    let p = window_price?;
    if !is_stable || p.lamports == 0 {
        return None;
    }
    // stables in this list have 6 decimals
    Some(p.quote_units as f64 / 1e6 / (p.lamports as f64 / 1e9))
}

// ─────────────────────────── Q3 ───────────────────────────

#[derive(Debug, Serialize)]
pub struct Q3Report {
    /// Cross-venue submissions found in the window (landed + failed).
    pub attempts: usize,
    pub landed: usize,
    pub failed: usize,
    /// Land rate permille. NOTE: this is a LAND rate (confirmed/submitted),
    /// not a win rate — S15B's Q3 lesson, kept explicit in the field name.
    pub land_rate_permille: u32,
    /// Distinct signers among LANDED cross-venue txs (failed txs are not
    /// fetched; their signers are unknown to this report).
    pub distinct_landed_signers: usize,
}

pub fn q3_aggregate(census: &Census, accts: &[TxAccounting]) -> Q3Report {
    let landed = census.cross.iter().filter(|t| !t.err).count();
    let failed = census.cross.len() - landed;
    let attempts = census.cross.len();
    let signers: BTreeSet<&str> = accts.iter().map(|a| a.signer.as_str()).collect();
    Q3Report {
        attempts,
        landed,
        failed,
        land_rate_permille: (landed * 1000).checked_div(attempts).unwrap_or(0) as u32,
        distinct_landed_signers: signers.len(),
    }
}

// ─────────────────────────── Q1 ───────────────────────────

/// One unit of Q1 work, derived deterministically from the input.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Q1Task {
    pub sig: String,
    pub slot: u64,
    /// Sorted, deduped pool set (matches the original bin's `BTreeSet`).
    pub pools: Vec<String>,
}

/// The Q1 work list from an input's evidence transactions, in evidence order.
/// This is the regression surface for the v1→v2 refactor: identical plans +
/// the single shared fetch/classify implementation ⇒ identical Q1 output.
pub fn q1_plan(input: &InputV2) -> Vec<Q1Task> {
    input.evidence.iter().map(q1_task_from_evidence).collect()
}

fn q1_task_from_evidence(t: &EvidenceTx) -> Q1Task {
    let set: BTreeSet<String> = t.pools.iter().cloned().collect();
    Q1Task {
        sig: t.sig.clone(),
        slot: t.slot,
        pools: set.into_iter().collect(),
    }
}

/// Q1 classification for one transaction. Field names and order are the
/// S15B originals — serialized reports stay byte-compatible.
#[derive(Debug, Clone, Serialize)]
pub struct Q1Row {
    pub sig: String,
    pub slot: u64,
    pub arb_index: Option<usize>,
    pub trigger_index: Option<usize>,
    pub trigger_signer: Option<String>,
    pub index_gap: Option<usize>,
    pub block_tx_count: usize,
    pub leader: Option<String>,
    pub prev_touch_slot: Option<u64>,
    pub prev_touch_sig: Option<String>,
    pub slot_gap: Option<u64>,
    pub class: String,
}

/// Pure classification: slot gap 0 ⇒ same-slot backrun; ≥1 ⇒ cross-slot.
pub fn q1_classify(slot_gap: Option<u64>) -> &'static str {
    match slot_gap {
        Some(0) => "SAME_SLOT_BACKRUN",
        Some(_) => "CROSS_SLOT",
        None => "UNCLEAR",
    }
}

/// Verdict thresholds from S15B: ≥70% same-slot ⇒ KILL, ≥30% cross-slot ⇒
/// PASS, else INCONCLUSIVE.
pub fn q1_verdict(same: usize, cross: usize, n: usize) -> &'static str {
    let n = n.max(1);
    if same * 100 / n >= 70 {
        "KILL"
    } else if cross * 100 / n >= 30 {
        "PASS"
    } else {
        "INCONCLUSIVE"
    }
}

/// Fetch the tx's block and locate (a) its index, (b) the nearest preceding
/// same-block tx touching its pools, (c) the block producer from rewards.
/// Ported verbatim from the S15B bin (including the self-match control).
pub fn fetch_block_analysis(rpc: &RpcClient, task: &Q1Task) -> Result<Q1Row, String> {
    use solana_client::rpc_config::RpcBlockConfig;
    use solana_transaction_status_client_types::{TransactionDetails, UiTransactionEncoding};
    let pools: BTreeSet<&String> = task.pools.iter().collect();
    let cfg = RpcBlockConfig {
        encoding: Some(UiTransactionEncoding::JsonParsed),
        transaction_details: Some(TransactionDetails::Full),
        rewards: Some(true), // block producer = Fee reward recipient
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };
    let block = with_retry(6, || {
        rpc.get_block_with_config(task.slot, cfg)
            .map_err(|e| anyhow::anyhow!("getBlock:{e}"))
    })
    .map_err(|e| e.to_string())?;
    let v = serde_json::to_value(&block).map_err(|_| "serialize".to_string())?;

    let leader = v["rewards"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|r| r["rewardType"].as_str() == Some("Fee"))
                .and_then(|r| r["pubkey"].as_str())
        })
        .map(str::to_string);

    let txs = v["transactions"].as_array().cloned().unwrap_or_default();
    let block_tx_count = txs.len();

    let arb_index = txs.iter().position(|x| {
        x["transaction"]["signatures"]
            .as_array()
            .and_then(|s| s.first())
            .and_then(|s| s.as_str())
            == Some(task.sig.as_str())
    });

    let touches = |x: &serde_json::Value| -> bool {
        x["transaction"]["message"]["accountKeys"]
            .as_array()
            .map(|ks| {
                ks.iter().any(|k| {
                    k["pubkey"]
                        .as_str()
                        .map(|p| pools.contains(&p.to_string()))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    };

    let (mut trigger_index, mut trigger_signer) = (None, None);
    if let Some(ai) = arb_index {
        for j in (0..ai).rev() {
            if touches(&txs[j]) {
                trigger_index = Some(j);
                trigger_signer = txs[j]["transaction"]["message"]["accountKeys"]
                    .as_array()
                    .and_then(|ks| ks.iter().find(|k| k["signer"].as_bool() == Some(true)))
                    .and_then(|k| k["pubkey"].as_str())
                    .map(str::to_string);
                break;
            }
        }
    }

    // CONTROL: the tx must match its OWN pools under the exact extraction used
    // to search for a trigger, else the matcher is broken (e.g. unresolved ALT
    // keys) and every CROSS_SLOT would be an artefact.
    let self_match = arb_index.map(|ai| touches(&txs[ai])).unwrap_or(false);
    if arb_index.is_some() && !self_match {
        return Err("SELF-MATCH CONTROL FAILED: tx does not match its own pools".into());
    }

    let (prev_touch_slot, prev_touch_sig) = nearest_preceding_pool_touch(rpc, task);
    let slot_gap = prev_touch_slot.map(|s| task.slot.saturating_sub(s));

    Ok(Q1Row {
        sig: task.sig.clone(),
        slot: task.slot,
        arb_index,
        trigger_index,
        trigger_signer,
        index_gap: match (arb_index, trigger_index) {
            (Some(a), Some(b)) => Some(a - b),
            _ => None,
        },
        block_tx_count,
        leader,
        prev_touch_slot,
        prev_touch_sig,
        slot_gap,
        class: q1_classify(slot_gap).to_string(),
    })
}

/// Nearest PRECEDING pool-touching tx across block boundaries, via each
/// pool's signature history anchored at the tx's own signature.
pub fn nearest_preceding_pool_touch(
    rpc: &RpcClient,
    task: &Q1Task,
) -> (Option<u64>, Option<String>) {
    let before = Signature::from_str(&task.sig).ok();
    let mut best: Option<(u64, String)> = None;
    for pool in &task.pools {
        let Ok(pk) = Pubkey::from_str(pool) else {
            continue;
        };
        let cfg = GetConfirmedSignaturesForAddress2Config {
            before,
            until: None,
            limit: Some(3),
            commitment: Some(CommitmentConfig::confirmed()),
        };
        if let Ok(sigs) = rpc.get_signatures_for_address_with_config(&pk, cfg) {
            if let Some(prev) = sigs.first() {
                if best.as_ref().is_none_or(|(s, _)| prev.slot > *s) {
                    best = Some((prev.slot, prev.signature.clone()));
                }
            }
        }
    }
    match best {
        Some((s, sig)) => (Some(s), Some(sig)),
        None => (None, None),
    }
}

#[derive(Debug, Serialize)]
pub struct Q1Report {
    pub n: usize,
    pub same_slot: usize,
    pub cross_slot: usize,
    pub unclear: usize,
    pub gap_min: Option<u64>,
    pub gap_median: Option<u64>,
    pub gap_p90: Option<u64>,
    pub gap_max: Option<u64>,
    pub verdict: String,
    pub rows: Vec<Q1Row>,
}

pub fn run_q1(rpc: &RpcClient, tasks: &[Q1Task]) -> Q1Report {
    let mut rows = Vec::new();
    for t in tasks {
        match fetch_block_analysis(rpc, t) {
            Ok(r) => rows.push(r),
            Err(e) => eprintln!("  q1 {}: FETCH FAILED: {e}", &t.sig[..12.min(t.sig.len())]),
        }
    }
    let same = rows
        .iter()
        .filter(|r| r.class == "SAME_SLOT_BACKRUN")
        .count();
    let cross = rows.iter().filter(|r| r.class == "CROSS_SLOT").count();
    let unclear = rows.len() - same - cross;
    let mut gaps: Vec<u64> = rows.iter().filter_map(|r| r.slot_gap).collect();
    gaps.sort_unstable();
    let pick = |i: usize| gaps.get(i).copied();
    Q1Report {
        n: rows.len(),
        same_slot: same,
        cross_slot: cross,
        unclear,
        gap_min: pick(0),
        gap_median: pick(gaps.len() / 2),
        gap_p90: pick((gaps.len() * 9 / 10).min(gaps.len().saturating_sub(1))),
        gap_max: gaps.last().copied(),
        verdict: q1_verdict(same, cross, rows.len()).to_string(),
        rows,
    }
}

// ─────────────────────────── Q2 ───────────────────────────

/// xorshift64* — deterministic, seedable, dependency-free. Statistical quality
/// is ample for p-value estimation at 10^4 trials.
struct XorShift64(u64);
impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

#[derive(Debug, Serialize)]
pub struct Q2Report {
    pub n_blocks: usize,
    pub distinct_leaders: usize,
    pub max_by_one_leader: usize,
    pub null_distinct_median: usize,
    pub null_max_median: usize,
    /// P(distinct ≤ observed) under stake-weighted null — LOW distinct count
    /// is what concentration looks like.
    pub p_distinct_le: f64,
    /// P(max ≥ observed) under the null.
    pub p_max_ge: f64,
    pub trials: usize,
    pub verdict: String,
}

/// Monte-Carlo test of leader concentration against a stake-weighted null.
/// Pure given the stake weights; deterministic (fixed seed).
pub fn q2_monte_carlo(leaders: &[String], stakes: &[(String, u64)], trials: usize) -> Q2Report {
    let n = leaders.len();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for l in leaders {
        *counts.entry(l.as_str()).or_default() += 1;
    }
    let obs_distinct = counts.len();
    let obs_max = counts.values().copied().max().unwrap_or(0);

    let total: u128 = stakes.iter().map(|(_, s)| *s as u128).sum();
    let mut cum: Vec<u128> = Vec::with_capacity(stakes.len());
    let mut acc = 0u128;
    for (_, s) in stakes {
        acc += *s as u128;
        cum.push(acc);
    }
    let mut rng = XorShift64(0x51B5_D15B_0000_0007);
    let mut d_le = 0usize;
    let mut m_ge = 0usize;
    let mut null_d = Vec::with_capacity(trials);
    let mut null_m = Vec::with_capacity(trials);
    for _ in 0..trials {
        let mut c: BTreeMap<usize, usize> = BTreeMap::new();
        for _ in 0..n {
            // Modulo bias ≤ total/2^64 per draw — immaterial at these scales.
            let r = ((rng.next() as u128) << 64 | rng.next() as u128) % total.max(1);
            let idx = cum.partition_point(|&c| c <= r);
            *c.entry(idx).or_default() += 1;
        }
        let dd = c.len();
        let mm = c.values().copied().max().unwrap_or(0);
        null_d.push(dd);
        null_m.push(mm);
        if dd <= obs_distinct {
            d_le += 1;
        }
        if mm >= obs_max {
            m_ge += 1;
        }
    }
    null_d.sort_unstable();
    null_m.sort_unstable();
    let p_distinct_le = d_le as f64 / trials.max(1) as f64;
    let p_max_ge = m_ge as f64 / trials.max(1) as f64;
    // Concentration must show BOTH as few distinct leaders AND a dominating
    // max to reject the public-flow null at 5%.
    let verdict = if n == 0 {
        "INCONCLUSIVE"
    } else if p_distinct_le < 0.05 || p_max_ge < 0.05 {
        "CONCENTRATED"
    } else {
        "PASS"
    };
    Q2Report {
        n_blocks: n,
        distinct_leaders: obs_distinct,
        max_by_one_leader: obs_max,
        null_distinct_median: null_d.get(trials / 2).copied().unwrap_or(0),
        null_max_median: null_m.get(trials / 2).copied().unwrap_or(0),
        p_distinct_le,
        p_max_ge,
        trials,
        verdict: verdict.to_string(),
    }
}

/// Fetch stake weights (current + delinquent vote accounts).
pub fn fetch_stakes(rpc: &RpcClient) -> Result<Vec<(String, u64)>> {
    let va = with_retry(6, || {
        rpc.get_vote_accounts()
            .map_err(|e| anyhow::anyhow!("getVoteAccounts: {e}"))
    })?;
    let mut stakes: BTreeMap<String, u64> = BTreeMap::new();
    for v in va.current.iter().chain(va.delinquent.iter()) {
        *stakes.entry(v.node_pubkey.clone()).or_default() += v.activated_stake;
    }
    Ok(stakes.into_iter().collect())
}

// ─────────────────────────── full pair scan ───────────────────────────

#[derive(Debug, Serialize)]
pub struct PairOutcome {
    pub venue_a: String,
    pub venue_b: String,
    pub n_pool_pairs: usize,
    pub quote_mint: String,
    pub window_hours: f64,
    pub census_total_txs: usize,
    pub q3: Q3Report,
    pub q4: Q4Report,
    pub q1: Option<Q1Report>,
    pub q2: Option<Q2Report>,
    /// Anything that limits interpretation, stated rather than smoothed over.
    pub notes: Vec<String>,
}

pub struct ScanOptions {
    pub max_pages: usize,
    /// Hard cap on tx fetches; exceeding it is an ERROR (shrink the window),
    /// never a silent subsample.
    pub max_tx_fetches: usize,
    /// Cap on Q1/Q2 block fetches over the top value-positive events.
    /// 0 disables Q1/Q2.
    pub q12_top_k: usize,
    pub q2_trials: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_pages: 120,
            max_tx_fetches: 12_000,
            q12_top_k: 20,
            q2_trials: 10_000,
        }
    }
}

/// Run the full pipeline over one input. Read-only throughout.
pub fn scan_pair(rpc: &RpcClient, input: &InputV2, opt: &ScanOptions) -> Result<PairOutcome> {
    input.validate()?;
    let quote = input.quote_mint()?.to_string();
    let mut notes = Vec::new();

    let census = census(rpc, input, opt.max_pages)?;
    let landed: Vec<&CensusTx> = census.cross.iter().filter(|t| !t.err).collect();
    if landed.len() > opt.max_tx_fetches {
        bail!(
            "{} landed cross-venue txs exceeds the {}-fetch cap; shrink the slot \
             window rather than subsampling",
            landed.len(),
            opt.max_tx_fetches
        );
    }

    let mut accts: Vec<TxAccounting> = Vec::with_capacity(landed.len());
    let mut fetch_failures = 0usize;
    for t in &landed {
        match fetch_accounting(rpc, &t.sig, t.slot) {
            Ok(a) => accts.push(a),
            Err(_) => fetch_failures += 1,
        }
    }
    if fetch_failures > 0 {
        notes.push(format!(
            "{fetch_failures}/{} landed txs could not be fetched/accounted; they are \
             excluded from Q4's numerator AND stated here rather than estimated",
            landed.len()
        ));
    }

    let pts = extract_price_points(&accts, &quote);
    let price_ok = quote != WSOL_MINT && pts.len() >= MIN_PRICE_POINTS;
    let pnls: Vec<(usize, Pnl)> = accts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let local = if price_ok {
                local_price(&pts, a.slot, PRICE_WINDOW_K)
            } else {
                None
            };
            (i, classify_value(a, &quote, local.as_ref()))
        })
        .collect();

    let q3 = q3_aggregate(&census, &accts);
    let q4 = q4_aggregate(&census, &accts, &pnls, &quote, input.window_hours, &pts);

    // Q1/Q2 over the top-K value-positive events (or the evidence set if the
    // input carries one — the S15B reproduction path).
    let (q1, q2) = if opt.q12_top_k == 0 {
        (None, None)
    } else {
        let tasks: Vec<Q1Task> = if !input.evidence.is_empty() {
            q1_plan(input)
        } else {
            let mut pos: Vec<(i128, usize)> = pnls
                .iter()
                .filter_map(|(i, p)| match p {
                    Pnl::Priced(v) if *v > 0 => Some((*v, *i)),
                    _ => None,
                })
                .collect();
            pos.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
            pos.iter()
                .take(opt.q12_top_k)
                .map(|(_, i)| {
                    let sig = &accts[*i].sig;
                    let ct = census.cross.iter().find(|t| &t.sig == sig);
                    Q1Task {
                        sig: sig.clone(),
                        slot: accts[*i].slot,
                        pools: ct.map(|t| t.pools.clone()).unwrap_or_default(),
                    }
                })
                .collect()
        };
        if tasks.is_empty() {
            notes.push("no value-positive events → Q1/Q2 skipped (nothing to measure)".into());
            (None, None)
        } else {
            let q1 = run_q1(rpc, &tasks);
            let leaders: Vec<String> = q1.rows.iter().filter_map(|r| r.leader.clone()).collect();
            let q2 = match fetch_stakes(rpc) {
                Ok(stakes) => Some(q2_monte_carlo(&leaders, &stakes, opt.q2_trials)),
                Err(e) => {
                    notes.push(format!("Q2 skipped: {e}"));
                    None
                }
            };
            (Some(q1), q2)
        }
    };

    Ok(PairOutcome {
        venue_a: input.venue_a.clone(),
        venue_b: input.venue_b.clone(),
        n_pool_pairs: input.pools.len(),
        quote_mint: quote,
        window_hours: input.window_hours,
        census_total_txs: census.total_txs,
        q3,
        q4,
        q1,
        q2,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the A1 regression guard ──────────────────────────────────────────
    // Inline copy of the ORIGINAL v1 parsing exactly as forensics_s15b.rs
    // shipped it (struct shape + BTreeSet pool extraction). The new loader
    // must produce a byte-identical Q1 work list from the same fixture:
    // identical plans + the single shared fetch/classify implementation ⇒
    // identical Q1 output.
    mod legacy {
        use serde::Deserialize;
        #[derive(Deserialize)]
        pub struct Input {
            pub transactions: Vec<InTx>,
        }
        #[derive(Deserialize)]
        pub struct InTx {
            pub sig: String,
            pub slot: u64,
            pub pools: Vec<PoolRef>,
        }
        #[derive(Deserialize)]
        pub struct PoolRef {
            pub pool: Option<String>,
        }
    }

    const V1_FIXTURE: &str = include_str!("../../fixtures/forensics/s15b_input.json");

    #[test]
    fn converted_v1_fixture_yields_byte_identical_q1_plan() {
        // Legacy path: exactly what the original bin computed per tx.
        let legacy: legacy::Input = serde_json::from_str(V1_FIXTURE).unwrap();
        let legacy_plan: Vec<Q1Task> = legacy
            .transactions
            .iter()
            .map(|t| {
                let set: std::collections::BTreeSet<String> =
                    t.pools.iter().filter_map(|p| p.pool.clone()).collect();
                Q1Task {
                    sig: t.sig.clone(),
                    slot: t.slot,
                    pools: set.into_iter().collect(),
                }
            })
            .collect();

        // New path: v1 → v2 conversion → q1_plan.
        let v2 = crate::forensics::schema::load_input(V1_FIXTURE).unwrap();
        let new_plan = q1_plan(&v2);

        assert_eq!(legacy_plan.len(), 45);
        assert_eq!(
            serde_json::to_string(&legacy_plan).unwrap(),
            serde_json::to_string(&new_plan).unwrap(),
            "v1→v2 conversion changed the Q1 work list"
        );
    }

    #[test]
    fn q1_classification_thresholds() {
        assert_eq!(q1_classify(Some(0)), "SAME_SLOT_BACKRUN");
        assert_eq!(q1_classify(Some(1)), "CROSS_SLOT");
        assert_eq!(q1_classify(Some(51)), "CROSS_SLOT");
        assert_eq!(q1_classify(None), "UNCLEAR");
        // S15B actuals: 5 same / 40 cross of 45 → PASS
        assert_eq!(q1_verdict(5, 40, 45), "PASS");
        assert_eq!(q1_verdict(32, 13, 45), "KILL");
        assert_eq!(q1_verdict(10, 10, 45), "INCONCLUSIVE");
    }

    fn acct(d_sol: i128, deltas: &[(&str, i128)]) -> TxAccounting {
        TxAccounting {
            sig: "test".into(),
            slot: 100,
            signer: "S".into(),
            fee: 5000,
            d_sol,
            external_native_inflow: 0,
            deltas: deltas.iter().map(|(m, d)| (m.to_string(), *d)).collect(),
        }
    }

    /// REGRESSION — the exact on-chain numbers from `3Ph3HGfiauYz…`
    /// (meteora-dlmm+pump-amm CTPoyCwk market): the signer gained 391,073,819
    /// lamports, of which 57,406,080 came from a non-token account
    /// (`CxCuYh6Q…`, the operator's own capital) — true arb value is
    /// 333,667,739 (== pool-vault WSOL outflow minus router fees).
    #[test]
    fn external_capital_inflow_is_not_arb_profit() {
        let mut a = acct(391_073_819, &[]);
        a.external_native_inflow = 57_406_080;
        assert_eq!(
            classify_value(&a, WSOL_MINT, None),
            Pnl::Priced(333_667_739)
        );
    }

    /// A PURE consolidation transfer (one of 11/85 found in the first batch
    /// run, e.g. `63X4P3BRzA…`: naive +564,406, inflow 569,438) must read as
    /// exactly the fee paid — never as a win.
    #[test]
    fn pure_capital_transfer_reads_as_minus_fee() {
        let mut a = acct(564_406, &[]);
        a.external_native_inflow = 569_438;
        match classify_value(&a, WSOL_MINT, None) {
            Pnl::Priced(v) => assert_eq!(v, -5_032, "strict value must be -fee"),
            _ => panic!("priceable"),
        }
    }

    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    #[test]
    fn classify_cycle_and_purchase_and_unpriceable() {
        let market = PricePoint {
            slot: 100,
            quote_units: 73_250_000,
            lamports: 1_000_000_000,
        };
        // Inventory-neutral cycle → priced as its SOL delta.
        let c = acct(50_000, &[]);
        assert_eq!(classify_value(&c, USDC, Some(&market)), Pnl::Priced(50_000));
        // SOL purchase (the S15B artefact): must come out NEGATIVE.
        let buy = acct(9_747_693, &[(USDC, -740_000)]);
        match classify_value(&buy, USDC, Some(&market)) {
            Pnl::Priced(v) => assert!(v < 0, "purchase priced {v}, must be a loss"),
            _ => panic!("priceable"),
        }
        // Third-mint delta → Unpriceable, never estimated.
        let other = acct(1_000_000, &[("SomeMemecoinMint111", 42)]);
        assert_eq!(
            classify_value(&other, USDC, Some(&market)),
            Pnl::Unpriceable
        );
        // Quote delta but no price available → Unpriceable.
        assert_eq!(classify_value(&buy, USDC, None), Pnl::Unpriceable);
        // Quote == WSOL: only cycles priceable.
        assert_eq!(classify_value(&c, WSOL_MINT, None), Pnl::Priced(50_000));
        assert_eq!(classify_value(&other, WSOL_MINT, None), Pnl::Unpriceable);
    }

    #[test]
    fn account_tx_extracts_signer_deltas() {
        let v = serde_json::json!({
            "slot": 42,
            "meta": {
                "fee": 5000,
                "preBalances": [1_000_000_000u64, 7],
                "postBalances": [1_002_458_110u64, 7],
                "preTokenBalances": [
                    {"accountIndex": 3, "mint": USDC, "owner": "SIGNER",
                     "uiTokenAmount": {"amount": "1000000"}},
                    {"accountIndex": 4, "mint": USDC, "owner": "POOL",
                     "uiTokenAmount": {"amount": "999"}}
                ],
                "postTokenBalances": [
                    {"accountIndex": 3, "mint": USDC, "owner": "SIGNER",
                     "uiTokenAmount": {"amount": "808365"}},
                    {"accountIndex": 4, "mint": USDC, "owner": "POOL",
                     "uiTokenAmount": {"amount": "1999"}}
                ]
            },
            "transaction": {"message": {"accountKeys": [
                {"pubkey": "SIGNER", "signer": true},
                {"pubkey": "OTHER", "signer": false}
            ]}}
        });
        let a = account_tx("sig1", &v).unwrap();
        assert_eq!(a.signer, "SIGNER");
        assert_eq!(a.d_sol, 2_458_110);
        assert_eq!(
            a.deltas.get(USDC),
            Some(&-191_635i128),
            "pool-owned balances excluded"
        );
        assert_eq!(a.slot, 42);
        assert_eq!(a.fee, 5000);
    }

    #[test]
    fn price_points_require_two_sided_clean_swaps() {
        let accts = vec![
            // clean sell of SOL for USDC → a point
            acct(-10_000_000, &[(USDC, 749_500)]),
            // one-sided (no quote) → not a point
            acct(50_000, &[]),
            // same-direction (both positive) → not a point
            acct(1_000, &[(USDC, 1_000)]),
            // third mint present → not a point
            acct(-10_000_000, &[(USDC, 749_500), ("Meme", 1)]),
        ];
        let pts = extract_price_points(&accts, USDC);
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].quote_units, 749_500);
        assert_eq!(pts[0].lamports, 10_000_000);
        assert!(extract_price_points(&accts, WSOL_MINT).is_empty());
    }

    #[test]
    fn q2_null_is_calibrated_and_detects_concentration() {
        // 200 equal-stake validators, 30 observed blocks.
        let stakes: Vec<(String, u64)> = (0..200).map(|i| (format!("v{i}"), 1_000)).collect();
        // Null-like draw: 30 distinct leaders.
        let dispersed: Vec<String> = (0..30).map(|i| format!("v{i}")).collect();
        let r = q2_monte_carlo(&dispersed, &stakes, 4_000);
        assert_eq!(r.verdict, "PASS", "dispersed leaders must pass: {r:?}");
        // One leader produced 15 of 30 blocks — flagrant concentration.
        let mut conc: Vec<String> = vec!["v0".into(); 15];
        conc.extend((1..16).map(|i| format!("v{i}")));
        let r = q2_monte_carlo(&conc, &stakes, 4_000);
        assert_eq!(r.verdict, "CONCENTRATED", "{r:?}");
    }

    #[test]
    fn q4_thresholds_and_concentration() {
        let census = Census {
            total_txs: 10,
            cross: vec![],
            per_pool_counts: BTreeMap::new(),
        };
        let accts: Vec<TxAccounting> = (0..6)
            .map(|i| {
                let mut a = acct(0, &[]);
                a.signer = format!("s{}", i % 2);
                a
            })
            .collect();
        // values: one large (100k), four small (6k), one negative
        let pnls: Vec<(usize, Pnl)> = vec![
            (0, Pnl::Priced(100_000)),
            (1, Pnl::Priced(6_000)),
            (2, Pnl::Priced(6_000)),
            (3, Pnl::Priced(6_000)),
            (4, Pnl::Priced(6_000)),
            (5, Pnl::Priced(-500)),
        ];
        let q4 = q4_aggregate(&census, &accts, &pnls, USDC, 24.0, &[]);
        assert_eq!(q4.value_positive, 5);
        assert_eq!(q4.max_positive_lamports, 100_000);
        assert_eq!(q4.below_sig_fee_floor, 0);
        let t50k = q4
            .thresholds
            .iter()
            .find(|t| t.threshold_lamports == 50_000)
            .unwrap();
        assert_eq!(t50k.events, 1);
        assert_eq!(t50k.sum_lamports, 100_000);
        let t0 = q4
            .thresholds
            .iter()
            .find(|t| t.threshold_lamports == 0)
            .unwrap();
        assert_eq!(t0.events, 5);
        assert_eq!(t0.events_per_day as i64, 5);
        // top 1% of 5 events = max(1) event = 100k of 124k total ≈ 806‰
        assert_eq!(q4.concentration_permille[0], 806);
        assert!(q4.usd_per_sol.is_none(), "no price points → no USD claim");
        assert!(q4.pricing_mode.starts_with("cycles-only"));
    }

    #[test]
    fn usd_only_from_stable_quotes() {
        let p = PricePoint {
            slot: 0,
            quote_units: 74_950_000,
            lamports: 1_000_000_000,
        };
        let u = usd_per_sol(USDC, Some(&p)).unwrap();
        assert!((u - 74.95).abs() < 1e-9);
        assert!(usd_per_sol(WSOL_MINT, Some(&p)).is_none());
        assert!(usd_per_sol("RandomMint", Some(&p)).is_none());
        assert!(usd_per_sol(USDC, None).is_none());
    }
}
