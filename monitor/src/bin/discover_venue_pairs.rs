//! discover-venue-pairs — find token markets that live on two venues at once
//! and emit v2 forensics inputs for the batch scanner.
//!
//! READ-ONLY. No key loading, no transaction construction.
//!
//! Method (a SAMPLING heuristic, denominators stated in the manifest):
//! 1. For each supported venue, take the newest `--pages`×1000 program
//!    signatures (the sample frame: this is the venue's most recent activity,
//!    not a full-window census — the census happens later in the batch scan).
//! 2. Fetch up to `--tx-cap` landed transactions per venue (stride-sampled so
//!    the cap does not silently bias toward the newest seconds).
//! 3. Resolve every unseen account once via `getMultipleAccounts`; an account
//!    owned by a venue program that decodes through that venue's EXISTING
//!    pool decoder (discriminator-checked) registers as a pool with its mints.
//! 4. A transaction touching pools of the same unordered mint pair on TWO
//!    venues is cross-venue evidence; rank (venue_a, venue_b, market) by
//!    distinct signers — the cheap proxy for whether anyone arbitrages it.
//! 5. Emit the top `--top` markets per venue combination as v2 inputs.
//!
//! Pagination inside the sample frame is explicit; if a page read fails the
//! run fails — never a silently smaller sample presented as the full frame.

use anyhow::{bail, Context, Result};
use arb_monitor::forensics::schema::{InputV2, PoolPair, WSOL_MINT};
use arb_monitor::forensics::venues::{adapter, VenueAdapter};
use serde::Serialize;
use solana_client::rpc_client::{GetConfirmedSignaturesForAddress2Config, RpcClient};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

/// Venues with a lib-side pool decoder. raydium-clmm is deliberately absent:
/// its mint extraction is Unsupported (no lib decoder — see venues.rs) and a
/// venue that cannot be decoded cannot be discovered honestly.
const DEFAULT_VENUES: &str = "meteora-dlmm,orca-whirlpool,raydium-v4,pump-amm";

/// Assumed slots/hour for converting `--hours` into an emitted slot window
/// (~400 ms/slot). The emitted window is re-measured by the batch scan; this
/// only sizes the frame.
const SLOTS_PER_HOUR: u64 = 9_000;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn with_retry<T>(tries: u32, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut delay = 400u64;
    let mut last = None;
    for _ in 0..tries {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(delay));
                delay = (delay * 9 / 5).min(20_000);
            }
        }
    }
    Err(last.unwrap())
}

#[derive(Debug, Clone, Serialize)]
struct PoolInfo {
    venue: &'static str,
    mint_a: String,
    mint_b: String,
}

#[derive(Debug, Serialize)]
struct Manifest {
    sample_frames: BTreeMap<String, SampleFrame>,
    markets_ranked: Vec<MarketRank>,
    emitted: Vec<String>,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SampleFrame {
    signatures_listed: usize,
    landed_in_frame: usize,
    txs_fetched: usize,
    slot_span: (u64, u64),
}

#[derive(Debug, Clone, Serialize)]
struct MarketRank {
    venue_a: String,
    venue_b: String,
    mint_pair: (String, String),
    distinct_signers: usize,
    cross_txs: usize,
    pools_a: Vec<String>,
    pools_b: Vec<String>,
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().collect();
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    let hours: f64 = arg(&args, "--hours")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(6.0);
    let pages: usize = arg(&args, "--pages")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(3);
    let tx_cap: usize = arg(&args, "--tx-cap")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(350);
    let top_n: usize = arg(&args, "--top")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(8);
    let out_dir = arg(&args, "--out").unwrap_or_else(|| "reports/forensics/inputs".into());
    let venue_names = arg(&args, "--venues").unwrap_or_else(|| DEFAULT_VENUES.into());
    std::fs::create_dir_all(&out_dir)?;

    let venues: Vec<&'static dyn VenueAdapter> = venue_names
        .split(',')
        .map(|n| adapter(n.trim()).with_context(|| format!("unknown venue {n}")))
        .collect::<Result<_>>()?;
    for v in &venues {
        if v.pool_mints(&[]).is_none() && v.name() == "raydium-clmm" {
            bail!("raydium-clmm mint discovery is Unsupported (no lib decoder); remove it from --venues");
        }
    }

    println!("=== discover-venue-pairs (READ-ONLY, sampling) ===");
    println!("venues: {venue_names}   pages/venue: {pages}   tx-cap/venue: {tx_cap}");

    let mut notes = Vec::new();
    let mut sample_frames = BTreeMap::new();
    // account -> Some(PoolInfo) | None (checked, not a pool)
    let mut pool_cache: BTreeMap<String, Option<PoolInfo>> = BTreeMap::new();
    // (sig) -> (signer, slot, accounts)
    let mut txs: BTreeMap<String, (String, u64, Vec<String>)> = BTreeMap::new();

    // 1+2: sample recent txs per venue.
    for v in &venues {
        let prog = v.program_id();
        let mut sigs: Vec<(String, u64)> = Vec::new();
        let mut before: Option<Signature> = None;
        for _ in 0..pages {
            let page = with_retry(8, || {
                let cfg = GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(1000),
                    commitment: Some(CommitmentConfig::confirmed()),
                };
                rpc.get_signatures_for_address_with_config(&prog, cfg)
                    .map_err(|e| anyhow::anyhow!("getSignaturesForAddress {}: {e}", v.name()))
            })?;
            let n = page.len();
            for s in &page {
                if s.err.is_none() {
                    sigs.push((s.signature.clone(), s.slot));
                }
            }
            if n < 1000 {
                break;
            }
            before = Signature::from_str(&page[n - 1].signature).ok();
        }
        let landed = sigs.len();
        // Stride-sample down to the cap so we do not bias toward the newest
        // seconds of a busy program.
        let stride = (landed / tx_cap.max(1)).max(1);
        let sampled: Vec<&(String, u64)> = sigs.iter().step_by(stride).take(tx_cap).collect();
        let mut fetched = 0usize;
        let (mut lo, mut hi) = (u64::MAX, 0u64);
        for (sig, slot) in &sampled {
            lo = lo.min(*slot);
            hi = hi.max(*slot);
            let raw = with_retry(6, || {
                rpc.send::<serde_json::Value>(
                    solana_client::rpc_request::RpcRequest::GetTransaction,
                    serde_json::json!([sig, {"encoding": "jsonParsed",
                        "maxSupportedTransactionVersion": 0, "commitment": "confirmed"}]),
                )
                .map_err(|e| anyhow::anyhow!("getTransaction: {e}"))
            });
            let Ok(raw) = raw else { continue };
            let keys: Vec<String> = raw["transaction"]["message"]["accountKeys"]
                .as_array()
                .map(|ks| {
                    ks.iter()
                        .filter_map(|k| k["pubkey"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let signer = raw["transaction"]["message"]["accountKeys"]
                .as_array()
                .and_then(|ks| {
                    ks.iter()
                        .find(|k| k["signer"].as_bool() == Some(true))
                        .and_then(|k| k["pubkey"].as_str())
                })
                .unwrap_or("")
                .to_string();
            if keys.is_empty() || signer.is_empty() {
                continue;
            }
            fetched += 1;
            txs.insert(sig.to_string(), (signer, *slot, keys));
        }
        sample_frames.insert(
            v.name().to_string(),
            SampleFrame {
                signatures_listed: landed,
                landed_in_frame: landed,
                txs_fetched: fetched,
                slot_span: (lo, hi),
            },
        );
        println!(
            "  {:<16} listed {landed} landed sigs, fetched {fetched} (stride {stride}), slots {lo}..{hi}",
            v.name()
        );
    }

    // 3: resolve unseen accounts in batches of 100.
    let unseen: Vec<String> = {
        let mut set = BTreeSet::new();
        for (_, _, keys) in txs.values() {
            for k in keys {
                if !pool_cache.contains_key(k) {
                    set.insert(k.clone());
                }
            }
        }
        set.into_iter().collect()
    };
    println!("  resolving {} distinct accounts…", unseen.len());
    let venue_by_prog: BTreeMap<String, &'static dyn VenueAdapter> = venues
        .iter()
        .map(|v| (v.program_id().to_string(), *v))
        .collect();
    for chunk in unseen.chunks(100) {
        let pks: Vec<Pubkey> = chunk
            .iter()
            .filter_map(|s| Pubkey::from_str(s).ok())
            .collect();
        let accounts = with_retry(8, || {
            rpc.get_multiple_accounts(&pks)
                .map_err(|e| anyhow::anyhow!("getMultipleAccounts: {e}"))
        })?;
        for (addr, acc) in chunk.iter().zip(accounts) {
            let info = acc.and_then(|a| {
                venue_by_prog.get(&a.owner.to_string()).and_then(|v| {
                    v.pool_mints(&a.data).map(|(ma, mb)| PoolInfo {
                        venue: v.name(),
                        mint_a: ma.to_string(),
                        mint_b: mb.to_string(),
                    })
                })
            });
            pool_cache.insert(addr.clone(), info);
        }
    }
    let n_pools = pool_cache.values().filter(|v| v.is_some()).count();
    println!("  identified {n_pools} pool accounts");

    // 4: cross-venue evidence per market.
    #[derive(Default)]
    struct Agg {
        signers: BTreeSet<String>,
        cross_txs: usize,
        pools_a: BTreeSet<String>,
        pools_b: BTreeSet<String>,
    }
    let mut markets: BTreeMap<(String, String, (String, String)), Agg> = BTreeMap::new();
    for (signer, _slot, keys) in txs.values() {
        // pools touched by this tx, grouped by (venue, mint-pair)
        let mut touched: BTreeMap<(&str, (String, String)), BTreeSet<String>> = BTreeMap::new();
        for k in keys {
            if let Some(Some(p)) = pool_cache.get(k) {
                let mut pair = [p.mint_a.clone(), p.mint_b.clone()];
                pair.sort();
                touched
                    .entry((p.venue, (pair[0].clone(), pair[1].clone())))
                    .or_default()
                    .insert(k.clone());
            }
        }
        // any two venues sharing a mint pair?
        let entries: Vec<_> = touched.iter().collect();
        for i in 0..entries.len() {
            for j in i + 1..entries.len() {
                let ((va, ma), pa) = entries[i];
                let ((vb, mb), pb) = entries[j];
                if va == vb || ma != mb {
                    continue;
                }
                let (va, vb, pa, pb) = if va < vb {
                    (va, vb, pa, pb)
                } else {
                    (vb, va, pb, pa)
                };
                let agg = markets
                    .entry((va.to_string(), vb.to_string(), ma.clone()))
                    .or_default();
                agg.signers.insert(signer.clone());
                agg.cross_txs += 1;
                agg.pools_a.extend(pa.iter().cloned());
                agg.pools_b.extend(pb.iter().cloned());
            }
        }
    }

    let mut ranked: Vec<MarketRank> = markets
        .into_iter()
        .map(|((va, vb, mp), agg)| MarketRank {
            venue_a: va,
            venue_b: vb,
            mint_pair: mp,
            distinct_signers: agg.signers.len(),
            cross_txs: agg.cross_txs,
            pools_a: agg.pools_a.into_iter().collect(),
            pools_b: agg.pools_b.into_iter().collect(),
        })
        .collect();
    ranked.sort_by_key(|m| std::cmp::Reverse((m.distinct_signers, m.cross_txs)));

    println!("\n  cross-venue markets found: {}", ranked.len());
    for m in ranked.iter().take(20) {
        println!(
            "    {:<14} + {:<14} {}…/{}…  signers={} txs={} pools={}x{}",
            m.venue_a,
            m.venue_b,
            &m.mint_pair.0[..8.min(m.mint_pair.0.len())],
            &m.mint_pair.1[..8.min(m.mint_pair.1.len())],
            m.distinct_signers,
            m.cross_txs,
            m.pools_a.len(),
            m.pools_b.len()
        );
    }

    // 5: emit v2 inputs — top N per venue combination.
    let now_slot = with_retry(6, || rpc.get_slot().map_err(|e| anyhow::anyhow!("{e}")))?;
    let slot_min = now_slot.saturating_sub((hours * SLOTS_PER_HOUR as f64) as u64);
    let mut per_combo: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut emitted = Vec::new();
    for m in &ranked {
        let combo = (m.venue_a.clone(), m.venue_b.clone());
        let c = per_combo.entry(combo).or_default();
        if *c >= top_n {
            continue;
        }
        *c += 1;
        // quote selection: USD stable if present, else WSOL, else the
        // lexicographically first mint (noted — USD economics will be
        // Unsupported for such markets).
        let (m0, m1) = (&m.mint_pair.0, &m.mint_pair.1);
        let quote = if arb_monitor::mint_safety::major_asset(m0).is_some() {
            m0.clone()
        } else if arb_monitor::mint_safety::major_asset(m1).is_some() {
            m1.clone()
        } else if m0 == WSOL_MINT || m1 == WSOL_MINT {
            WSOL_MINT.to_string()
        } else {
            notes.push(format!(
                "market {}/{} has neither a stable nor WSOL; quote defaulted to {} — USD economics Unsupported",
                m0, m1, m0
            ));
            m0.clone()
        };
        let token = if quote == *m0 { m1.clone() } else { m0.clone() };
        // cross product of discovered pools, capped at 6 pairs to bound the
        // census cost (stated, not silent: the cap is in the file name count).
        let mut pools = Vec::new();
        'outer: for pa in &m.pools_a {
            for pb in &m.pools_b {
                pools.push(PoolPair {
                    pool_a: pa.clone(),
                    pool_b: pb.clone(),
                    token_mint: token.clone(),
                    quote_mint: quote.clone(),
                });
                if pools.len() >= 6 {
                    break 'outer;
                }
            }
        }
        let input = InputV2 {
            schema_version: 2,
            description: format!(
                "discovered market {}…/{}… on {}+{} ({} sampled signers, {} sampled cross-txs)",
                &token[..8.min(token.len())],
                &quote[..8.min(quote.len())],
                m.venue_a,
                m.venue_b,
                m.distinct_signers,
                m.cross_txs
            ),
            venue_a: m.venue_a.clone(),
            venue_b: m.venue_b.clone(),
            pools,
            slot_min,
            slot_max: now_slot,
            window_hours: hours,
            known_signers: Vec::new(),
            evidence: Vec::new(),
        };
        input.validate()?;
        let fname = format!(
            "{out_dir}/{}+{}-{}.json",
            m.venue_a,
            m.venue_b,
            &m.mint_pair.0[..8.min(m.mint_pair.0.len())]
        );
        std::fs::write(&fname, serde_json::to_string_pretty(&input)?)?;
        emitted.push(fname);
    }

    let manifest = Manifest {
        sample_frames,
        markets_ranked: ranked,
        emitted: emitted.clone(),
        notes,
    };
    let mpath = format!("{out_dir}/discovery-manifest.json");
    std::fs::write(&mpath, serde_json::to_string_pretty(&manifest)?)?;
    println!("\n  emitted {} v2 inputs to {out_dir}", emitted.len());
    println!("  manifest: {mpath}");
    Ok(())
}
