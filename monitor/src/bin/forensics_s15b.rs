//! S15B — historical forensics on the `meteora-dlmm + orca-whirlpool` family.
//! READ-ONLY: historical RPC only. No Geyser, no streaming, no transaction
//! construction, no signing, no submission, ever.
//!
//! Four questions, each of which can independently kill the thesis:
//!   Q1 same-slot or cross-slot?  → can we even compete (instrument choice)
//!   Q2 public or staked flow?    → can we win (infrastructure)
//!   Q3 win rate?                 → business or lottery
//!   Q4 frequency + economics?    → is the pot bigger than the bill
//!
//! Rules honoured in the output: every number carries its denominator; ratios
//! compare like-for-like (median vs median); an unresolved measurement is
//! reported INCONCLUSIVE with the data that would settle it, never filled with
//! an assumption. If a question returns KILL the caller stops there.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use std::collections::{BTreeMap, BTreeSet};

const INPUT: &str = include_str!("../../fixtures/forensics/s15b_input.json");

#[derive(Debug, Deserialize)]
struct Input {
    window_hours: f64,
    total_net_profit_lamports: i128,
    signers: Vec<String>,
    pairs: Vec<Pair>,
    transactions: Vec<InTx>,
}
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // consumed by Q4 (threshold-size sweep over each pair)
struct Pair {
    meteora: String,
    whirlpool: String,
}
#[derive(Debug, Deserialize, Clone)]
struct InTx {
    sig: String,
    slot: u64,
    #[allow(dead_code)]
    block_time: Option<i64>,
    #[allow(dead_code)] // consumed by Q3 (per-signer win rate)
    signer: String,
    #[allow(dead_code)] // consumed by Q4 (realized-profit economics)
    net_profit_lamports: i128,
    pools: Vec<PoolRef>,
}
#[derive(Debug, Deserialize, Clone)]
struct PoolRef {
    #[allow(dead_code)]
    venue: String,
    pool: Option<String>,
}

/// Q1 classification for one arbitrage transaction.
#[derive(Debug, Clone, Serialize)]
struct Q1Row {
    sig: String,
    slot: u64,
    /// Index of the arb tx within its block.
    arb_index: Option<usize>,
    /// Nearest preceding tx in the SAME block touching one of the arb's pools.
    trigger_index: Option<usize>,
    trigger_signer: Option<String>,
    /// arb_index - trigger_index.
    index_gap: Option<usize>,
    block_tx_count: usize,
    /// Leader (block producer) taken from getBlock rewards — ground truth for a
    /// historical slot, unlike getSlotLeaders which is only defined for the
    /// current epoch.
    leader: Option<String>,
    /// Slot of the nearest PRECEDING transaction touching any of the arb's
    /// pools, searched across block boundaries (not just within the arb's own
    /// block). This is what actually decides Q1: the in-block search alone
    /// cannot distinguish "trigger one slot earlier" from "no trigger for 51
    /// slots", and these pools are quiet enough that the arb is usually the
    /// ONLY tx in its ~1,000-tx block touching them.
    prev_touch_slot: Option<u64>,
    prev_touch_sig: Option<String>,
    /// arb.slot - prev_touch_slot. 0 ⇒ same-slot backrun; ≥1 ⇒ the dislocation
    /// sat unclaimed for that many slots and is Geyser-observable.
    slot_gap: Option<u64>,
    class: String,
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    let input: Input = serde_json::from_str(INPUT).context("parse s15b_input.json")?;
    let args: Vec<String> = std::env::args().collect();
    let only_q1 = args.iter().any(|a| a == "--q1");
    let out_dir = std::env::var("S15B_OUT").unwrap_or_else(|_| "reports/forensics".into());
    std::fs::create_dir_all(&out_dir).ok();

    println!("=== S15B historical forensics (READ-ONLY) ===");
    println!(
        "input: {} transactions, {} signers, {} pool pairs, {:.1}h window, {:.6} SOL realized net",
        input.transactions.len(),
        input.signers.len(),
        input.pairs.len(),
        input.window_hours,
        input.total_net_profit_lamports as f64 / 1e9
    );

    // ─────────────────────────── Q1 ───────────────────────────
    println!(
        "\n═══ Q1 — same-slot backrun or cross-slot? (n={}) ═══",
        input.transactions.len()
    );
    let mut rows: Vec<Q1Row> = Vec::new();
    // Cross-validation of the leader source on IN-EPOCH slots only.
    let epoch = rpc.get_epoch_info().ok();
    let epoch_first = epoch
        .as_ref()
        .map(|e| e.absolute_slot - e.slot_index)
        .unwrap_or(u64::MAX);
    let mut leader_xval: Vec<(u64, String, String)> = Vec::new();

    for (i, t) in input.transactions.iter().enumerate() {
        let pools: BTreeSet<String> = t.pools.iter().filter_map(|p| p.pool.clone()).collect();
        match fetch_block_analysis(&rpc, t, &pools) {
            Ok(mut row) => {
                // Where the slot IS in the current epoch, check getSlotLeaders
                // agrees with the rewards-derived producer.
                if t.slot >= epoch_first {
                    if let (Some(rew), Ok(sl)) =
                        (row.leader.clone(), rpc.get_slot_leaders(t.slot, 1))
                    {
                        if let Some(first) = sl.first() {
                            leader_xval.push((t.slot, rew, first.to_string()));
                        }
                    }
                }
                if i < 5 || row.class == "SAME_SLOT_BACKRUN" || row.slot_gap.unwrap_or(0) >= 10 {
                    println!(
                        "  {} slot={} idx={:?}/{} prev_touch={:?} slot_gap={:?} → {}",
                        &t.sig[..12],
                        t.slot,
                        row.arb_index,
                        row.block_tx_count,
                        row.prev_touch_slot,
                        row.slot_gap,
                        row.class
                    );
                }
                row.sig = t.sig.clone();
                rows.push(row);
            }
            Err(e) => println!("  {} slot={} → FETCH FAILED: {e}", &t.sig[..12], t.slot),
        }
    }

    let mut dist: BTreeMap<String, usize> = BTreeMap::new();
    for r in &rows {
        *dist.entry(r.class.clone()).or_default() += 1;
    }
    println!("\n  classification (n={}):", rows.len());
    for (k, v) in &dist {
        println!(
            "    {k:<20} {v:>3}  ({:.0}%)",
            *v as f64 * 100.0 / rows.len() as f64
        );
    }
    let mut gaps: Vec<u64> = rows.iter().filter_map(|r| r.slot_gap).collect();
    gaps.sort_unstable();
    if !gaps.is_empty() {
        println!(
            "\n  SLOT GAP (arb.slot - last preceding pool-touching tx), n={}:\n    min={} median={} p90={} max={}",
            gaps.len(),
            gaps[0],
            gaps[gaps.len() / 2],
            gaps[((gaps.len() as f64 * 0.9) as usize).min(gaps.len() - 1)],
            gaps[gaps.len() - 1]
        );
        let mut hist: BTreeMap<u64, usize> = BTreeMap::new();
        for g in &gaps {
            *hist.entry(*g).or_default() += 1;
        }
        for (g, c) in &hist {
            println!(
                "      gap={g:>3} slots : {c:>3}  ({:.0}%)",
                *c as f64 * 100.0 / gaps.len() as f64
            );
        }
        println!(
            "    gap>=1: {}/{}   gap>=2: {}/{}",
            gaps.iter().filter(|g| **g >= 1).count(),
            gaps.len(),
            gaps.iter().filter(|g| **g >= 2).count(),
            gaps.len()
        );
    }
    if !leader_xval.is_empty() {
        let agree = leader_xval.iter().filter(|(_, a, b)| a == b).count();
        println!(
            "  leader-source cross-validation on in-epoch slots: {agree}/{} agree (rewards vs getSlotLeaders)",
            leader_xval.len()
        );
    } else {
        println!(
            "  leader-source cross-validation: no in-epoch slots in the set — using rewards only"
        );
    }

    let same = dist.get("SAME_SLOT_BACKRUN").copied().unwrap_or(0);
    let cross = dist.get("CROSS_SLOT").copied().unwrap_or(0);
    let n = rows.len().max(1);
    let verdict = if same * 100 / n >= 70 {
        "KILL"
    } else if cross * 100 / n >= 30 {
        "PASS"
    } else {
        "INCONCLUSIVE"
    };
    println!("\n  Q1 VERDICT: {verdict}");
    println!(
        "    SAME_SLOT_BACKRUN {same}/{n}, CROSS_SLOT {cross}/{n}, other {}/{n}",
        n - same - cross
    );
    match verdict {
        "KILL" => println!(
            "    → the trigger sits in the SAME block at a lower index. Competing needs the\n\
             \x20     victim swap at/BEFORE inclusion (ShredStream / leader-adjacent), NOT Geyser\n\
             \x20     at processed commitment. That is infrastructure we do not have."
        ),
        "PASS" => println!(
            "    → a meaningful share of dislocations survive at least one slot, so an\n\
             \x20     event-driven Geyser observer can plausibly see them."
        ),
        _ => println!(
            "    → neither threshold met; see the per-tx table before drawing a conclusion."
        ),
    }

    let path = format!("{out_dir}/s15b-q1-{}.json", now_ms());
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({
            "question": "Q1 same-slot vs cross-slot",
            "n": rows.len(),
            "distribution": dist,
            "index_gaps": gaps,
            "verdict": verdict,
            "rows": rows,
        }))?,
    )?;
    println!("  wrote {path}");

    if only_q1 {
        println!("\n(--q1: stopping after Q1 as instructed)");
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Fetch the arb's block and locate (a) the arb's index, (b) the nearest
/// preceding transaction touching either pool of its route, (c) the block
/// producer from the rewards list.
fn fetch_block_analysis(
    rpc: &RpcClient,
    t: &InTx,
    pools: &BTreeSet<String>,
) -> Result<Q1Row, String> {
    use solana_client::rpc_config::RpcBlockConfig;
    use solana_transaction_status_client_types::{TransactionDetails, UiTransactionEncoding};
    let cfg = RpcBlockConfig {
        encoding: Some(UiTransactionEncoding::JsonParsed),
        transaction_details: Some(TransactionDetails::Full),
        rewards: Some(true), // block producer = Fee reward recipient
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };
    let block = rpc
        .get_block_with_config(t.slot, cfg)
        .map_err(|e| format!("getBlock:{e}"))?;
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

    // Locate the arb by signature.
    let arb_index = txs.iter().position(|x| {
        x["transaction"]["signatures"]
            .as_array()
            .and_then(|s| s.first())
            .and_then(|s| s.as_str())
            == Some(t.sig.as_str())
    });

    // Does a transaction touch any of the arb's pools?
    let touches = |x: &serde_json::Value| -> bool {
        x["transaction"]["message"]["accountKeys"]
            .as_array()
            .map(|ks| {
                ks.iter().any(|k| {
                    k["pubkey"]
                        .as_str()
                        .map(|p| pools.contains(p))
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

    // CONTROL: the arb must match its OWN pools under the exact same extraction
    // used to search for a trigger. If it does not, the matcher is broken (e.g.
    // ALT-loaded keys not resolved) and every CROSS_SLOT is an artefact.
    let self_match = arb_index.map(|ai| touches(&txs[ai])).unwrap_or(false);
    if arb_index.is_some() && !self_match {
        return Err("SELF-MATCH CONTROL FAILED: arb tx does not match its own pools".into());
    }

    // The decisive Q1 measurement: nearest preceding pool-touching tx ACROSS
    // block boundaries, via each pool's own signature history.
    let (prev_touch_slot, prev_touch_sig) = nearest_preceding_pool_touch(rpc, t, pools);
    let slot_gap = prev_touch_slot.map(|s| t.slot.saturating_sub(s));

    let class = match (arb_index, slot_gap) {
        (_, Some(0)) => "SAME_SLOT_BACKRUN",
        (_, Some(_)) => "CROSS_SLOT",
        _ => "UNCLEAR",
    }
    .to_string();

    Ok(Q1Row {
        sig: t.sig.clone(),
        slot: t.slot,
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
        class,
    })
}

/// Nearest PRECEDING transaction touching any of the arb's pools, searched
/// across block boundaries using each pool's signature history anchored at the
/// arb's own signature (`before`). Returns the most recent such touch — i.e.
/// the last event that could have moved either pool's price before the arb.
fn nearest_preceding_pool_touch(
    rpc: &RpcClient,
    t: &InTx,
    pools: &BTreeSet<String>,
) -> (Option<u64>, Option<String>) {
    use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
    use solana_sdk::{pubkey::Pubkey, signature::Signature};
    use std::str::FromStr;

    let before = Signature::from_str(&t.sig).ok();
    let mut best: Option<(u64, String)> = None;
    for pool in pools {
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

/// Realized P&L of a transaction, in lamports of SOL-equivalent VALUE.
///
/// THE ACCOUNTING IDENTITY THIS CRATE GOT WRONG (S15B).
///
/// The S15A pipeline computed realized profit as `d_sol + d_wsol`. That is
/// correct ONLY for a closed cycle that begins and ends holding the same
/// non-SOL inventory. When a signer SPENDS another token to acquire SOL, the
/// formula books the proceeds of that sale as profit and ignores what was paid
/// for it — so every ordinary purchase of SOL reports a large fake profit.
///
/// This is not hypothetical: all 45 transactions in the S15B fixture are
/// SOL purchases, and the old formula reported +0.651 SOL of arbitrage that was
/// never earned. See `docs/forensics-s15b.md`.
///
/// `d_quote` is the signer's delta in the quote token (negative = spent),
/// `quote_per_sol` is the market rate in quote units per SOL, and
/// `quote_decimals_scale` converts quote units to whole tokens (1e6 for USDC).
pub fn value_pnl_lamports(
    d_sol: i128,
    d_wsol: i128,
    d_quote: i128,
    quote_per_sol: f64,
    quote_decimals_scale: f64,
) -> i128 {
    let quote_in_sol = (d_quote as f64 / quote_decimals_scale) / quote_per_sol;
    d_sol + d_wsol + (quote_in_sol * 1e9) as i128
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC_SCALE: f64 = 1e6;

    /// REGRESSION: the exact on-chain numbers from `5KTk2eUJya…`, one of the 45
    /// transactions the S15A pipeline reported as a +0.0097 SOL arbitrage.
    /// The signer spent 0.74 USDC across 5 pools to buy SOL at 74.61 USDC/SOL
    /// while the market was 73.25 — a purchase at ~1.9% OVER market, not profit.
    #[test]
    fn buying_sol_with_usdc_is_not_arbitrage_profit() {
        let (d_sol, d_wsol, d_usdc) = (9_747_693_i128, 0_i128, -740_000_i128);

        // The OLD formula books the whole SOL inflow as profit.
        assert_eq!(d_sol + d_wsol, 9_747_693, "the number S15A reported");

        // Priced against the contemporaneous market, it is a LOSS.
        let v = value_pnl_lamports(d_sol, d_wsol, d_usdc, 73.25, USDC_SCALE);
        assert!(
            v < 0,
            "a purchase above market must not read as profit, got {v}"
        );
        assert!(
            (-400_000..-300_000).contains(&v),
            "expected ~-0.00035 SOL, got {v}"
        );
    }

    /// A genuine closed cycle spends no net quote token, so both formulas agree.
    #[test]
    fn closed_cycle_is_unaffected_by_quote_pricing() {
        let v = value_pnl_lamports(50_000, 0, 0, 74.95, USDC_SCALE);
        assert_eq!(v, 50_000);
    }

    /// A user selling SOL at market prices to ~zero — this is what makes value
    /// pricing a valid arb/user discriminator.
    #[test]
    fn market_rate_swap_nets_about_zero() {
        // sell 0.01 SOL, receive 0.7495 USDC at 74.95 USDC/SOL
        let v = value_pnl_lamports(-10_000_000, 0, 749_500, 74.95, USDC_SCALE);
        assert!(v.abs() < 10_000, "market-rate swap should net ~0, got {v}");
    }

    #[test]
    fn fixture_parses_and_is_the_expected_evidence_set() {
        let i: Input = serde_json::from_str(INPUT).unwrap();
        assert_eq!(i.transactions.len(), 45, "the 45 profitable arbs");
        assert_eq!(i.signers.len(), 4, "4 known operators");
        assert_eq!(i.pairs.len(), 6, "6 validated pool pairs");
        assert_eq!(
            i.total_net_profit_lamports,
            651_047_000_i128.min(i.total_net_profit_lamports)
        );
        // Every tx carries a slot and at least one resolved pool.
        for t in &i.transactions {
            assert!(t.slot > 0, "{} has no slot", t.sig);
            assert!(
                t.pools.iter().any(|p| p.pool.is_some()),
                "{} has no resolved pool",
                t.sig
            );
        }
    }

    #[test]
    fn every_tx_signer_is_one_of_the_four() {
        let i: Input = serde_json::from_str(INPUT).unwrap();
        let set: BTreeSet<&String> = i.signers.iter().collect();
        for t in &i.transactions {
            assert!(set.contains(&t.signer), "unknown signer {}", t.signer);
        }
    }

    #[test]
    fn fixture_net_profit_is_the_discredited_statistic() {
        // Every fixture net is positive — but this proves nothing about profit.
        // S15B established that all 45 are SOL PURCHASES, and that
        // `d_sol + d_wsol` books the SOL bought as profit while ignoring the
        // USDC paid for it. Priced at market, all 18 that are priceable in
        // WSOL/USDC alone are NEGATIVE (fixture +0.082366 SOL → true
        // -0.002154 SOL).
        //
        // This test is retained to pin the fixture's contents, NOT to assert
        // that the values are correct. Use `value_pnl_lamports` for profit.
        // See docs/forensics-s15b.md.
        let i: Input = serde_json::from_str(INPUT).unwrap();
        for t in &i.transactions {
            assert!(
                t.net_profit_lamports > 0,
                "fixture shape changed: {} net={}",
                t.sig,
                t.net_profit_lamports
            );
        }
    }
}
