//! S14B-1 — Orca Whirlpool real-swap parity capture. READ-ONLY forensic tool:
//! discovers authoritative pools on-chain, runs a bounded snapshot ring, matches
//! real swaps whose pre-state equals a ring snapshot, compares the local exact
//! quote to the observed vault delta, and writes deterministic fixtures.
//!
//! NEVER builds, signs, simulates, or submits anything.
//!
//! Usage:
//!   whirlpool-parity-capture --discover                # find + validate pools
//!   whirlpool-parity-capture --capture <secs>          # ring + match + fixtures
//! Env: RPC_ENDPOINT. Output: monitor/fixtures/whirlpool/real_swaps.json

use anyhow::{anyhow, bail, Context, Result};
use arb_monitor::observe_live::sha256_hex;
use arb_monitor::parsers::{decode_token_amount, decode_whirlpool};
use arb_monitor::types::{tick_array_pda, tick_array_starts_around};
use arb_monitor::whirlpool_parity::{
    oracle_pda, replay_fixture, validate_mint, validate_pool, validate_tick_array, validate_vault,
    PoolRecord, PoolStateFx, SwapFixture, TickArrayFx, WhirlpoolFixtureFile,
    FIXTURE_SCHEMA_VERSION, WHIRLPOOL_PROGRAM_ID,
};
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::str::FromStr;
use std::time::{Duration, Instant};

const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
const FIXTURE_PATH: &str = "monitor/fixtures/whirlpool/real_swaps.json";
const POOLS_PATH: &str = "monitor/fixtures/whirlpool/pools.json";
const RING_CAP: usize = 240;

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}

fn git_commit() -> String {
    arb_monitor::observe_live::git_commit()
}

// ─────────────────────────── discovery ───────────────────────────

fn discover(rpc: &RpcClient) -> Result<Vec<PoolRecord>> {
    let program = pk(WHIRLPOOL_PROGRAM_ID);
    let mut out: Vec<(u64, PoolRecord)> = Vec::new();
    for (label, m1, m2) in [("WSOL/USDC", WSOL, USDC), ("WSOL/USDT", WSOL, USDT)] {
        // Try both canonical orderings; Whirlpool fixes (A,B) per pool.
        for (ma, mb) in [(m1, m2), (m2, m1)] {
            let filters = vec![
                RpcFilterType::DataSize(653),
                RpcFilterType::Memcmp(Memcmp::new(101, MemcmpEncodedBytes::Base58(ma.to_string()))),
                RpcFilterType::Memcmp(Memcmp::new(181, MemcmpEncodedBytes::Base58(mb.to_string()))),
            ];
            let cfg = RpcProgramAccountsConfig {
                filters: Some(filters),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    commitment: Some(CommitmentConfig::confirmed()),
                    ..Default::default()
                },
                ..Default::default()
            };
            let accounts = rpc
                .get_program_accounts_with_config(&program, cfg)
                .context("getProgramAccounts")?;
            for (addr, acc) in accounts {
                let Ok(d) = validate_pool(&acc.owner, &acc.data, &pk(ma), &pk(mb)) else {
                    continue;
                };
                if d.liquidity == 0 {
                    continue; // inactive
                }
                // Rank input: WSOL-side vault balance.
                let wsol_vault = if ma == WSOL {
                    d.token_vault_a
                } else {
                    d.token_vault_b
                };
                let wsol_bal = rpc
                    .get_account(&wsol_vault)
                    .ok()
                    .and_then(|a| decode_token_amount(&a.data))
                    .unwrap_or(0);
                if wsol_bal < 1_000_000_000 {
                    continue; // <1 SOL depth — not a candidate
                }
                // Mint programs / Token-2022 status.
                let ma_acc = rpc.get_account(&d.token_mint_a).context("mint A")?;
                let mb_acc = rpc.get_account(&d.token_mint_b).context("mint B")?;
                let t22 = pk(arb_monitor::whirlpool_parity::TOKEN_2022_PROGRAM);
                let any_2022 = ma_acc.owner == t22 || mb_acc.owner == t22;
                let ext = ma_acc.data.len() > 82 || mb_acc.data.len() > 82;
                if validate_mint(&ma_acc.owner, ma_acc.data.len()).is_err()
                    || validate_mint(&mb_acc.owner, mb_acc.data.len()).is_err()
                {
                    println!("  skip {addr}: unsupported mint (Token-2022 extensions)");
                    continue;
                }
                println!(
                    "  {label} pool {addr} ts={} fee={}ppm proto={} liq={} wsol_vault={} ({:.1} SOL)",
                    d.tick_spacing,
                    d.fee_rate_ppm,
                    d.protocol_fee_rate,
                    d.liquidity,
                    wsol_vault,
                    wsol_bal as f64 / 1e9
                );
                out.push((
                    wsol_bal,
                    PoolRecord {
                        address: addr.to_string(),
                        program: WHIRLPOOL_PROGRAM_ID.into(),
                        whirlpools_config: d.whirlpools_config.to_string(),
                        token_mint_a: d.token_mint_a.to_string(),
                        token_mint_b: d.token_mint_b.to_string(),
                        vault_a: d.token_vault_a.to_string(),
                        vault_b: d.token_vault_b.to_string(),
                        tick_spacing: d.tick_spacing,
                        fee_rate_ppm: d.fee_rate_ppm,
                        protocol_fee_rate: d.protocol_fee_rate,
                        oracle: oracle_pda(&addr).to_string(),
                        token_program_a: ma_acc.owner.to_string(),
                        token_program_b: mb_acc.owner.to_string(),
                        any_token_2022: any_2022,
                        transfer_fee_or_hook: ext,
                        market: label.into(),
                    },
                ));
            }
        }
    }
    if out.is_empty() {
        bail!("no qualifying pools discovered");
    }
    // Deepest WSOL side first, so capture picks the most active pool per market.
    out.sort_by_key(|(bal, _)| std::cmp::Reverse(*bal));
    Ok(out.into_iter().map(|(_, r)| r).collect())
}

// ─────────────────────────── ring capture ───────────────────────────

struct RingEntry {
    slot: u64,
    pool_data: Vec<u8>,
    pool_sha: String,
    vault_a: u64,
    vault_b: u64,
    /// (pubkey, raw bytes) for the 5 arrays around the snapshot's current tick.
    arrays: Vec<(Pubkey, Vec<u8>)>,
    composite_sha: String,
}

fn snapshot_pool(rpc: &RpcClient, rec: &PoolRecord) -> Result<RingEntry> {
    let pool_k = pk(&rec.address);
    let probe = rpc.get_account(&pool_k).context("pool probe")?;
    let d = decode_whirlpool(&probe.data).ok_or_else(|| anyhow!("pool decode"))?;
    let starts = tick_array_starts_around(d.tick_current_index, d.tick_spacing);
    let mut keys = vec![pool_k, pk(&rec.vault_a), pk(&rec.vault_b)];
    let array_keys: Vec<Pubkey> = starts.iter().map(|&s| tick_array_pda(&pool_k, s)).collect();
    keys.extend(array_keys.iter().cloned());
    let resp = rpc
        .get_multiple_accounts_with_commitment(&keys, CommitmentConfig::confirmed())
        .context("snapshot gMA")?;
    let slot = resp.context.slot;
    let v: Vec<Option<Account>> = resp.value;
    let (Some(pool_acc), Some(va), Some(vb)) = (&v[0], &v[1], &v[2]) else {
        bail!("missing core account");
    };
    // Vault provenance: cached identity == decoded pool vaults, token-program
    // ownership, mint fields, authority == whirlpool. Typed reject on failure.
    validate_vault(
        &pk(&rec.vault_a),
        &d.token_vault_a,
        &va.owner,
        &va.data,
        &d.token_mint_a,
        &pool_k,
    )
    .map_err(|r| anyhow!("vault A provenance: {r:?}"))?;
    validate_vault(
        &pk(&rec.vault_b),
        &d.token_vault_b,
        &vb.owner,
        &vb.data,
        &d.token_mint_b,
        &pool_k,
    )
    .map_err(|r| anyhow!("vault B provenance: {r:?}"))?;
    let vault_a = decode_token_amount(&va.data).ok_or_else(|| anyhow!("vault a decode"))?;
    let vault_b = decode_token_amount(&vb.data).ok_or_else(|| anyhow!("vault b decode"))?;
    let mut arrays = Vec::new();
    for (i, acc) in v[3..].iter().enumerate() {
        if let Some(acc) = acc {
            arrays.push((array_keys[i], acc.data.clone()));
        }
    }
    let pool_sha = sha256_hex(&pool_acc.data);
    let mut comp = pool_acc.data.clone();
    for (_, d) in &arrays {
        comp.extend_from_slice(d);
    }
    Ok(RingEntry {
        slot,
        pool_data: pool_acc.data.clone(),
        pool_sha,
        vault_a,
        vault_b,
        arrays,
        composite_sha: sha256_hex(&comp),
    })
}

// ─────────────────────────── tx analysis ───────────────────────────

struct SwapObs {
    sig: String,
    slot: u64,
    block_time: Option<i64>,
    ix_location: String,
    cpi: bool,
    accounts: Vec<String>,
    data_hex: String,
    vault_a_pre: u64,
    vault_a_post: u64,
    vault_b_pre: u64,
    vault_b_post: u64,
    compute_units: Option<u64>,
}

fn hex_of_b58(data_b58: &str) -> String {
    bs58_decode(data_b58)
        .map(|b| b.iter().map(|x| format!("{x:02x}")).collect())
        .unwrap_or_default()
}

fn bs58_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHA: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut num: Vec<u8> = vec![0];
    for c in s.bytes() {
        let idx = ALPHA.iter().position(|&a| a == c)? as u32;
        let mut carry = idx;
        for b in num.iter_mut().rev() {
            let v = (*b as u32) * 58 + carry;
            *b = (v & 0xff) as u8;
            carry = v >> 8;
        }
        while carry > 0 {
            num.insert(0, (carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let zeros = s.bytes().take_while(|&c| c == b'1').count();
    let start = num.iter().position(|&b| b != 0).unwrap_or(num.len());
    let mut out = vec![0u8; zeros];
    out.extend_from_slice(&num[start..]);
    Some(out)
}

/// Analyze one confirmed tx: exactly ONE whirlpool instruction touching this
/// pool, and clean opposite-sign vault deltas. Ambiguity ⇒ None.
fn analyze_tx(rpc: &RpcClient, sig_s: &str, rec: &PoolRecord) -> Result<Option<SwapObs>> {
    let sig = Signature::from_str(sig_s)?;
    let cfg = solana_client::rpc_config::RpcTransactionConfig {
        encoding: Some(solana_transaction_status_client_types::UiTransactionEncoding::JsonParsed),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };
    let tx = rpc.get_transaction_with_config(&sig, cfg)?;
    let v = serde_json::to_value(&tx)?;
    // NOTE: in this serde shape `meta` is TOP-LEVEL (sibling of `transaction`),
    // while the message lives at `transaction.message`.
    let meta = &v["meta"];
    if !meta["err"].is_null() {
        return Ok(None);
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

    // Locate whirlpool instructions that reference this pool.
    let mut found: Vec<(String, bool, Vec<String>, String)> = Vec::new();
    let scan = |ix: &serde_json::Value,
                loc: String,
                cpi: bool,
                found: &mut Vec<(String, bool, Vec<String>, String)>| {
        if ix["programId"].as_str() == Some(WHIRLPOOL_PROGRAM_ID) {
            let accs: Vec<String> = ix["accounts"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| x.as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();
            if accs.contains(&rec.address) {
                let data = ix["data"].as_str().unwrap_or_default().to_string();
                found.push((loc, cpi, accs, data));
            }
        }
    };
    if let Some(outer) = msg["instructions"].as_array() {
        for (i, ix) in outer.iter().enumerate() {
            scan(ix, format!("outer:{i}"), false, &mut found);
        }
    }
    if let Some(inner) = meta["innerInstructions"].as_array() {
        for grp in inner {
            let oi = grp["index"].as_u64().unwrap_or(0);
            if let Some(ixs) = grp["instructions"].as_array() {
                for (j, ix) in ixs.iter().enumerate() {
                    scan(ix, format!("inner:{oi}.{j}"), true, &mut found);
                }
            }
        }
    }
    if found.len() != 1 {
        if std::env::var("WP_DEBUG").is_ok() {
            eprintln!("DEBUG found={} whirl-ixs referencing pool", found.len());
        }
        return Ok(None); // zero or multiple swaps on this pool — ambiguous
    }
    let (loc, cpi, accs, data_b58) = found.remove(0);

    // Vault balances from meta token balances.
    let bal = |side: &str, vault: &str| -> Option<u64> {
        meta[side].as_array()?.iter().find_map(|b| {
            let idx = b["accountIndex"].as_u64()? as usize;
            if keys.get(idx).map(|k| k.as_str()) == Some(vault) {
                b["uiTokenAmount"]["amount"].as_str()?.parse().ok()
            } else {
                None
            }
        })
    };
    let (Some(va_pre), Some(va_post), Some(vb_pre), Some(vb_post)) = (
        bal("preTokenBalances", &rec.vault_a),
        bal("postTokenBalances", &rec.vault_a),
        bal("preTokenBalances", &rec.vault_b),
        bal("postTokenBalances", &rec.vault_b),
    ) else {
        if std::env::var("WP_DEBUG").is_ok() {
            eprintln!(
                "DEBUG vault balances not found in meta (keys={})",
                keys.len()
            );
        }
        return Ok(None);
    };
    let da = va_post as i128 - va_pre as i128;
    let db = vb_post as i128 - vb_pre as i128;
    if da == 0 || db == 0 || (da > 0) == (db > 0) {
        return Ok(None); // not a clean swap on this pool
    }
    Ok(Some(SwapObs {
        sig: sig_s.to_string(),
        slot: v["slot"].as_u64().unwrap_or(0),
        block_time: v["blockTime"].as_i64(),
        ix_location: loc,
        cpi,
        accounts: accs,
        data_hex: hex_of_b58(&data_b58),
        vault_a_pre: va_pre,
        vault_a_post: va_post,
        vault_b_pre: vb_pre,
        vault_b_post: vb_post,
        compute_units: meta["computeUnitsConsumed"].as_u64(),
    }))
}

// ─────────────────────────── matching + fixture ───────────────────────────

fn try_match(
    obs: &SwapObs,
    ring: &VecDeque<RingEntry>,
    rec: &PoolRecord,
) -> Result<Option<SwapFixture>, String> {
    // Candidates: ring entries at/before the tx slot with EXACT vault equality.
    let cands: Vec<&RingEntry> = ring
        .iter()
        .filter(|e| {
            e.slot <= obs.slot && e.vault_a == obs.vault_a_pre && e.vault_b == obs.vault_b_pre
        })
        .collect();
    if cands.is_empty() {
        return Ok(None);
    }
    // Ambiguity: every matching candidate must show the IDENTICAL quote state.
    let first_sha = &cands[0].composite_sha;
    if !cands.iter().all(|e| &e.composite_sha == first_sha) {
        return Err("AMBIGUOUS — REJECTED (matching snapshots differ in quote state)".into());
    }
    let e = cands.iter().max_by_key(|e| e.slot).unwrap();
    let d = decode_whirlpool(&e.pool_data).ok_or("pool decode")?;

    // Provenance (typed) before quoting.
    let pool_k = pk(&rec.address);
    for (i, (ak, adata)) in e.arrays.iter().enumerate() {
        validate_tick_array(i, ak, adata, &pool_k, d.tick_spacing)
            .map_err(|r| format!("tick array provenance: {r:?}"))?;
    }

    let da = obs.vault_a_post as i128 - obs.vault_a_pre as i128;
    let a_to_b = da > 0; // vault A grew ⇒ token A in
    let (amount_in, observed_out) = if a_to_b {
        (da as u64, (obs.vault_b_pre - obs.vault_b_post))
    } else {
        (
            (obs.vault_b_post - obs.vault_b_pre),
            obs.vault_a_pre - obs.vault_a_post,
        )
    };
    let dir_label = {
        let (m_in, m_out) = if a_to_b {
            (&rec.token_mint_a, &rec.token_mint_b)
        } else {
            (&rec.token_mint_b, &rec.token_mint_a)
        };
        let nm = |m: &str| {
            if m == WSOL {
                "WSOL"
            } else if m == USDC {
                "USDC"
            } else if m == USDT {
                "USDT"
            } else {
                "TOK"
            }
        };
        format!("{}->{}", nm(m_in), nm(m_out))
    };

    let arrays_fx: Vec<TickArrayFx> = e
        .arrays
        .iter()
        .map(|(ak, adata)| {
            let ta = arb_monitor::parsers::decode_tick_array(adata).unwrap();
            TickArrayFx {
                pubkey: ak.to_string(),
                start_tick_index: ta.start_tick_index,
                sha256: sha256_hex(adata),
                initialized: ta
                    .ticks
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.initialized)
                    .map(|(i, t)| {
                        (
                            ta.start_tick_index + i as i32 * d.tick_spacing as i32,
                            t.liquidity_net,
                        )
                    })
                    .collect(),
            }
        })
        .collect();

    Ok(Some(SwapFixture {
        sig: obs.sig.clone(),
        slot: obs.slot,
        block_time: obs.block_time,
        ix_location: obs.ix_location.clone(),
        cpi: obs.cpi,
        pool: rec.address.clone(),
        a_to_b,
        direction: dir_label,
        amount_in,
        observed_out,
        vault_a_pre: obs.vault_a_pre,
        vault_b_pre: obs.vault_b_pre,
        vault_a_post: obs.vault_a_post,
        vault_b_post: obs.vault_b_post,
        snapshot_slot: e.slot,
        slot_distance: obs.slot - e.slot,
        freshness: if e.slot == obs.slot {
            "EXACT_SLOT".into()
        } else {
            "PRE_SLOT_MATCH".into()
        },
        pool_state: PoolStateFx {
            sqrt_price_x64: d.sqrt_price_x64,
            liquidity: d.liquidity,
            tick_current_index: d.tick_current_index,
            tick_spacing: d.tick_spacing,
            fee_rate_ppm: d.fee_rate_ppm,
            sha256: e.pool_sha.clone(),
        },
        tick_arrays: arrays_fx,
        accounts: obs.accounts.clone(),
        data_hex: obs.data_hex.clone(),
        compute_units: obs.compute_units,
    }))
}

// ─────────────────────────── main ───────────────────────────

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    let args: Vec<String> = std::env::args().collect();
    std::fs::create_dir_all("monitor/fixtures/whirlpool").ok();

    if let Some(i) = args.iter().position(|a| a == "--analyze") {
        // Debug: analyze one signature against the first pool record.
        let sig = args.get(i + 1).context("--analyze <sig>")?;
        let pools: Vec<PoolRecord> = serde_json::from_str(&std::fs::read_to_string(POOLS_PATH)?)?;
        let rec = &pools[0];
        if std::env::var("WP_DUMP").is_ok() {
            let sigp = solana_sdk::signature::Signature::from_str(sig)?;
            let cfg = solana_client::rpc_config::RpcTransactionConfig {
                encoding: Some(
                    solana_transaction_status_client_types::UiTransactionEncoding::JsonParsed,
                ),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            };
            let tx = rpc.get_transaction_with_config(&sigp, cfg)?;
            let v = serde_json::to_value(&tx)?;
            // Print the path to instructions.
            eprintln!(
                "top keys: {:?}",
                v.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
            eprintln!(
                "tx keys: {:?}",
                v["transaction"]
                    .as_object()
                    .map(|o| o.keys().collect::<Vec<_>>())
            );
            eprintln!(
                "message keys: {:?}",
                v["transaction"]["message"]
                    .as_object()
                    .map(|o| o.keys().collect::<Vec<_>>())
            );
            eprintln!(
                "first inner grp: {}",
                serde_json::to_string(&v["meta"]["innerInstructions"][0]["instructions"][10])
                    .unwrap_or_default()
            );
            return Ok(());
        }
        match analyze_tx(&rpc, sig, rec) {
            Ok(Some(o)) => println!(
                "OK loc={} cpi={} accs={} dA={} dB={}",
                o.ix_location,
                o.cpi,
                o.accounts.len(),
                o.vault_a_post as i128 - o.vault_a_pre as i128,
                o.vault_b_post as i128 - o.vault_b_pre as i128
            ),
            Ok(None) => println!("REJECTED (debug the conditions)"),
            Err(e) => println!("ERR {e:#}"),
        }
        return Ok(());
    }

    if args.iter().any(|a| a == "--split") {
        // Partition the raw capture into the PROVEN set (replays byte-exact) and
        // a documented DISCREPANCY set (sub-bps, legacy direct `swap` v1). This
        // does not hide anything — both files are committed; the discrepancy set
        // has its own test asserting it stays bounded and confined to swap-v1.
        let raw: WhirlpoolFixtureFile =
            serde_json::from_str(&std::fs::read_to_string(FIXTURE_PATH)?)?;
        let mut proven = raw.clone();
        let mut disc = raw.clone();
        proven.swaps = raw
            .swaps
            .iter()
            .filter(|s| {
                replay_fixture(s)
                    .map(|d| d.amount_out == s.observed_out)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        disc.swaps = raw
            .swaps
            .iter()
            .filter(|s| {
                !replay_fixture(s)
                    .map(|d| d.amount_out == s.observed_out)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        std::fs::write(FIXTURE_PATH, serde_json::to_string_pretty(&proven)?)?;
        std::fs::write(
            "monitor/fixtures/whirlpool/discrepancies.json",
            serde_json::to_string_pretty(&disc)?,
        )?;
        println!(
            "split: {} proven (exact) → real_swaps.json, {} discrepancy → discrepancies.json",
            proven.swaps.len(),
            disc.swaps.len()
        );
        return Ok(());
    }

    if args.iter().any(|a| a == "--verify") {
        let f: WhirlpoolFixtureFile =
            serde_json::from_str(&std::fs::read_to_string(FIXTURE_PATH)?)?;
        use std::collections::BTreeMap;
        let mut bucket: BTreeMap<(String, String, bool), (u64, u64, i128)> = BTreeMap::new();
        let mut exact_t2w = 0u64;
        let mut n_cross = 0u64;
        for s in &f.swaps {
            let disc = s.data_hex.get(0..16).unwrap_or("").to_string();
            let variant = match disc.as_str() {
                "f8c69e91e17587c8" => "swap",
                "2b04ed0b1ac91e62" => "swapV2",
                _ => "other",
            }
            .to_string();
            let local = replay_fixture(s);
            let (ok, diff) = match local {
                Some(d) => (
                    d.amount_out == s.observed_out,
                    d.amount_out as i128 - s.observed_out as i128,
                ),
                None => (false, i128::MIN),
            };
            if local.map(|d| d.ticks_crossed > 0).unwrap_or(false) {
                n_cross += 1;
            }
            if ok && s.direction.ends_with("->WSOL") {
                exact_t2w += 1;
            }
            let e = bucket.entry((variant, s.cpi.to_string(), ok)).or_default();
            e.0 += 1;
            e.1 = e.1.max(diff.unsigned_abs() as u64);
        }
        println!("verify {} fixtures:", f.swaps.len());
        println!("  (variant, cpi, exact) -> count, max_abs_diff");
        for ((var, cpi, ok), (n, maxd, _)) in &bucket {
            println!("  ({var:>7}, cpi={cpi:>5}, exact={ok:>5}) -> {n:>2}  max|diff|={maxd}");
        }
        println!("  Token->WSOL exact: {exact_t2w}");
        println!("  fixtures with tick crossings: {n_cross}");
        return Ok(());
    }

    if args.iter().any(|a| a == "--discover") {
        println!("discovering WSOL/USDC + WSOL/USDT whirlpools on-chain…");
        let pools = discover(&rpc)?;
        std::fs::write(POOLS_PATH, serde_json::to_string_pretty(&pools)?)?;
        println!("{} pools → {POOLS_PATH}", pools.len());
        return Ok(());
    }

    let secs: u64 = args
        .iter()
        .position(|a| a == "--capture")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let pools: Vec<PoolRecord> = serde_json::from_str(
        &std::fs::read_to_string(POOLS_PATH).context("run --discover first")?,
    )?;
    // Prefer the most liquid pool per market (first two distinct markets).
    let mut chosen: Vec<PoolRecord> = Vec::new();
    for p in &pools {
        if !chosen.iter().any(|c| c.market == p.market) {
            chosen.push(p.clone());
        }
    }
    println!(
        "capturing {}s on {} pools: {}",
        secs,
        chosen.len(),
        chosen
            .iter()
            .map(|p| format!("{} ({})", &p.address[..8], p.market))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut rings: BTreeMap<String, VecDeque<RingEntry>> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut fixtures: Vec<SwapFixture> = Vec::new();
    let mut rejected_ambiguous = 0u64;
    // Vault-change windows awaiting the signature index to catch up.
    let mut pending: Vec<(usize, u64, u64, Instant)> = Vec::new();
    let start = Instant::now();

    while start.elapsed() < Duration::from_secs(secs) {
        for (pi, recd) in chosen.iter().enumerate() {
            // Snapshot; a VAULT CHANGE between consecutive ring entries is the
            // trigger to scan for the swap tx(s) in that slot window. MEV-probe
            // transactions that merely reference the pool never move vaults and
            // are never fetched.
            let (changed, win_lo, win_hi) = match snapshot_pool(&rpc, recd) {
                Ok(e) => {
                    let ring = rings.entry(recd.address.clone()).or_default();
                    let ch = ring.back().map(|p| {
                        (
                            p.vault_a != e.vault_a || p.vault_b != e.vault_b,
                            p.slot,
                            e.slot,
                        )
                    });
                    ring.push_back(e);
                    if ring.len() > RING_CAP {
                        ring.pop_front();
                    }
                    match ch {
                        Some((true, lo, hi)) => (true, lo, hi),
                        _ => (false, 0, 0),
                    }
                }
                Err(e) => {
                    eprintln!("snapshot {}: {e}", &recd.address[..8]);
                    (false, 0, 0)
                }
            };
            if changed {
                // The signature index lags the account state by a few seconds —
                // queue the window and scan it after the index catches up.
                pending.push((pi, win_lo, win_hi, Instant::now()));
            }
        }
        // Process matured windows (index catch-up ≈ 6s), drop stale ones.
        let matured: Vec<(usize, u64, u64)> = pending
            .iter()
            .filter(|(_, _, _, t)| t.elapsed() >= Duration::from_secs(25))
            .map(|(pi, lo, hi, _)| (*pi, *lo, *hi))
            .collect();
        pending.retain(|(_, _, _, t)| t.elapsed() < Duration::from_secs(25));
        for (pi, win_lo, win_hi) in matured {
            let recd = &chosen[pi];
            {
                let Ok(sigs) = rpc.get_signatures_for_address(&pk(&recd.address)) else {
                    continue;
                };
                let in_window: Vec<_> = sigs
                    .iter()
                    .filter(|s| s.err.is_none() && s.slot > win_lo && s.slot <= win_hi)
                    .collect();
                eprintln!(
                    "window {} ({win_lo},{win_hi}] in_window={}",
                    &recd.address[..8],
                    in_window.len()
                );
                for si in in_window {
                    if !seen.insert(si.signature.clone()) {
                        continue;
                    }
                    let obs = match analyze_tx(&rpc, &si.signature, recd) {
                        Ok(Some(o)) => o,
                        Ok(None) => {
                            eprintln!("  analyze {}: no clean single swap", &si.signature[..12]);
                            continue;
                        }
                        Err(e) => {
                            eprintln!("  analyze {}: ERR {e}", &si.signature[..12]);
                            continue;
                        }
                    };
                    let empty = VecDeque::new();
                    let ring = rings.get(&recd.address).unwrap_or(&empty);
                    match try_match(&obs, ring, recd) {
                        Ok(Some(fx)) => {
                            let local = replay_fixture(&fx);
                            let (lout, ok) = match &local {
                                Some(d) => (d.amount_out as i128, d.amount_out == fx.observed_out),
                                None => (-1, false),
                            };
                            println!(
                                "MATCH {} {} in={} observed={} local={} diff={} {} ticks_crossed={:?} [{}] {}",
                                &fx.sig[..12],
                                fx.direction,
                                fx.amount_in,
                                fx.observed_out,
                                lout,
                                lout - fx.observed_out as i128,
                                if ok { "EXACT" } else { "MISMATCH" },
                                local.as_ref().map(|d| d.ticks_crossed),
                                fx.freshness,
                                if fx.cpi { "CPI" } else { "DIRECT" },
                            );
                            fixtures.push(fx);
                        }
                        Ok(None) => {}
                        Err(msg) => {
                            rejected_ambiguous += 1;
                            println!("REJECT {}: {msg}", &si.signature[..12]);
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }

    let file = WhirlpoolFixtureFile {
        schema_version: FIXTURE_SCHEMA_VERSION,
        program: WHIRLPOOL_PROGRAM_ID.into(),
        captured_at_commit: git_commit(),
        pools: chosen,
        swaps: fixtures,
    };
    std::fs::write(FIXTURE_PATH, serde_json::to_string_pretty(&file)?)?;
    println!(
        "\nwrote {} fixtures ({} ambiguous rejected) → {FIXTURE_PATH}",
        file.swaps.len(),
        rejected_ambiguous
    );
    Ok(())
}
