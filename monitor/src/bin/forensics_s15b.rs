//! S15B — historical forensics, now a thin wrapper over the generalized
//! `forensics` library module (see `monitor/src/forensics/`).
//! READ-ONLY: historical RPC only. No Geyser, no streaming, no transaction
//! construction, no signing, no submission, ever.
//!
//! The input is taken from `--input <path>` (v1 or v2 schema, auto-detected).
//! With no argument it runs the committed S15B fixture, reproducing the
//! published Q1 numbers: the v1→v2 conversion is guarded by a byte-identical
//! work-list regression test in `forensics::pipeline::tests`, and the
//! fetch/classify implementation is the single shared one in the library.

use anyhow::{Context, Result};
use arb_monitor::forensics::pipeline::{fetch_block_analysis, q1_plan, q1_verdict, Q1Row, Q1Task};
use arb_monitor::forensics::schema::load_input;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use std::collections::BTreeMap;

const DEFAULT_INPUT: &str = "monitor/fixtures/forensics/s15b_input.json";

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    let args: Vec<String> = std::env::args().collect();
    let only_q1 = args.iter().any(|a| a == "--q1");
    let input_path = args
        .iter()
        .position(|a| a == "--input")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .unwrap_or(DEFAULT_INPUT);
    let json =
        std::fs::read_to_string(input_path).with_context(|| format!("read input {input_path}"))?;
    let input = load_input(&json)?;
    let out_dir = std::env::var("S15B_OUT").unwrap_or_else(|_| "reports/forensics".into());
    std::fs::create_dir_all(&out_dir).ok();

    println!("=== S15B historical forensics (READ-ONLY) ===");
    println!(
        "input: {input_path} ({} + {}), {} evidence txs, {} pool pairs, {:.1}h window",
        input.venue_a,
        input.venue_b,
        input.evidence.len(),
        input.pools.len(),
        input.window_hours,
    );

    // ─────────────────────────── Q1 ───────────────────────────
    let tasks: Vec<Q1Task> = q1_plan(&input);
    println!(
        "\n═══ Q1 — same-slot backrun or cross-slot? (n={}) ═══",
        tasks.len()
    );
    let mut rows: Vec<Q1Row> = Vec::new();
    // Cross-validation of the leader source on IN-EPOCH slots only.
    let epoch = rpc.get_epoch_info().ok();
    let epoch_first = epoch
        .as_ref()
        .map(|e| e.absolute_slot - e.slot_index)
        .unwrap_or(u64::MAX);
    let mut leader_xval: Vec<(u64, String, String)> = Vec::new();

    for (i, t) in tasks.iter().enumerate() {
        match fetch_block_analysis(&rpc, t) {
            Ok(row) => {
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
    let verdict = q1_verdict(same, cross, rows.len());
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

#[cfg(test)]
mod tests {
    use arb_monitor::forensics::schema::{load_input, InputV1};
    use std::collections::BTreeSet;

    /// Test-only pin of the committed fixture's CONTENT. The runtime path is
    /// `--input`; this include_str exists solely so the pins cannot drift from
    /// the file the published numbers came from.
    const INPUT: &str = include_str!("../../fixtures/forensics/s15b_input.json");

    #[test]
    fn fixture_parses_and_is_the_expected_evidence_set() {
        let v2 = load_input(INPUT).unwrap();
        assert_eq!(v2.evidence.len(), 45, "the 45 evidence txs");
        assert_eq!(v2.known_signers.len(), 4, "4 known operators");
        assert_eq!(v2.pools.len(), 6, "6 validated pool pairs");
        for t in &v2.evidence {
            assert!(t.slot > 0, "{} has no slot", t.sig);
            assert!(!t.pools.is_empty(), "{} has no resolved pool", t.sig);
        }
    }

    #[test]
    fn every_tx_signer_is_one_of_the_four() {
        let v1: InputV1 = serde_json::from_str(INPUT).unwrap();
        let set: BTreeSet<&String> = v1.signers.iter().collect();
        for t in &v1.transactions {
            assert!(set.contains(&t.signer), "unknown signer {}", t.signer);
        }
    }

    #[test]
    fn fixture_net_profit_is_the_discredited_statistic() {
        // Every fixture net is positive — but this proves nothing about
        // profit. S15B established that all 45 are SOL PURCHASES: `d_sol +
        // d_wsol` books the SOL bought as profit and ignores the USDC paid.
        // Retained to pin fixture CONTENTS, not to assert correctness. Profit
        // lives in `forensics::price::value_pnl`. See docs/forensics-s15b.md.
        #[derive(serde::Deserialize)]
        struct Probe {
            transactions: Vec<TxProbe>,
        }
        #[derive(serde::Deserialize)]
        struct TxProbe {
            sig: String,
            net_profit_lamports: i128,
        }
        let p: Probe = serde_json::from_str(INPUT).unwrap();
        for t in &p.transactions {
            assert!(
                t.net_profit_lamports > 0,
                "fixture shape changed: {} net={}",
                t.sig,
                t.net_profit_lamports
            );
        }
    }
}
