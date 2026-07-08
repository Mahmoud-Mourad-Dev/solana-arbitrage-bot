//! discover-pools — build a richer `pools.generated.json` for cyclic
//! arbitrage from live Raydium AMM v4 + Orca Whirlpool listings.
//!
//! Fetches active pools, filters by liquidity / volume, then keeps ONLY pools
//! that can actually participate in a cycle anchored at a base mint (WSOL /
//! USDC): tokens must appear in >= 2 kept pools and the pool must sit in the
//! base-connected component. Isolated / dead / drained pools are dropped.
//! Writes `pools.generated.json` (same schema as pools.json) and prints a
//! summary. Never overwrites `pools.json`.
//!
//! Read-only: no keypair, no submit, no Jito, no Redis.
//!
//! Env:
//!   BASE_MINTS               cycle anchors (default WSOL) — from the usual .env
//!   DISCOVER_MIN_TVL_USD     default 25000
//!   DISCOVER_MAX_TVL_USD     default 0 (= no cap; set to exclude the deepest,
//!                            hyper-efficient majors)
//!   DISCOVER_MIN_VOL24H_USD  default 5000
//!   DISCOVER_MAX_POOLS       default 100 (final cap, ranked by volume)
//!   DISCOVER_MAX_HOPS        default 3 (cycle-length bound for the route count)
//!   RAYDIUM_PAGES            default 6 (x1000 pools/page, by 24h volume desc)
//!   POOLS_OUT                default pools.generated.json

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const RAYDIUM_AMM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

fn env_f64(k: &str, d: f64) -> f64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}
fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}
fn env_str(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| d.to_string())
}

fn symbol(mint: &str) -> &'static str {
    match mint {
        WSOL => "SOL",
        USDC => "USDC",
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => "USDT",
        "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R" => "RAY",
        "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So" => "mSOL",
        "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs" => "ETH",
        _ => "",
    }
}

#[derive(Clone)]
struct Candidate {
    address: String,
    dex: &'static str,
    mint_a: String,
    mint_b: String,
    tvl: f64,
    volume: f64,
    label: String,
}

fn pair_label(dex: &str, a: &str, b: &str) -> String {
    let sa = symbol(a);
    let sb = symbol(b);
    let sa = if sa.is_empty() { &a[..4] } else { sa };
    let sb = if sb.is_empty() { &b[..4] } else { sb };
    let venue = if dex == "raydium-v4" {
        "Raydium"
    } else {
        "Orca"
    };
    format!("{venue} {sa}/{sb}")
}

async fn fetch_raydium(
    client: &reqwest::Client,
    pages: usize,
    min_tvl: f64,
    max_tvl: f64,
    min_vol: f64,
) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for page in 1..=pages {
        let url = format!(
            "https://api-v3.raydium.io/pools/info/list?poolType=standard&poolSortField=volume24h&sortType=desc&pageSize=1000&page={page}"
        );
        let resp: Value = client
            .get(&url)
            .send()
            .await
            .context("raydium fetch")?
            .json()
            .await
            .context("raydium json")?;
        let arr = resp["data"]["data"].as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            break;
        }
        for p in arr {
            if p["type"].as_str() != Some("Standard")
                || p["programId"].as_str() != Some(RAYDIUM_AMM_V4)
            {
                continue;
            }
            let tvl = p["tvl"].as_f64().unwrap_or(0.0);
            let volume = p["day"]["volume"].as_f64().unwrap_or(0.0);
            if tvl < min_tvl || (max_tvl > 0.0 && tvl > max_tvl) || volume < min_vol {
                continue;
            }
            let (Some(addr), Some(a), Some(b)) = (
                p["id"].as_str(),
                p["mintA"]["address"].as_str(),
                p["mintB"]["address"].as_str(),
            ) else {
                continue;
            };
            out.push(Candidate {
                address: addr.to_string(),
                dex: "raydium-v4",
                mint_a: a.to_string(),
                mint_b: b.to_string(),
                tvl,
                volume,
                label: pair_label("raydium-v4", a, b),
            });
        }
    }
    Ok(out)
}

async fn fetch_orca(
    client: &reqwest::Client,
    min_tvl: f64,
    max_tvl: f64,
    min_vol: f64,
) -> Result<Vec<Candidate>> {
    let resp: Value = client
        .get("https://api.mainnet.orca.so/v1/whirlpool/list")
        .send()
        .await
        .context("orca fetch")?
        .json()
        .await
        .context("orca json")?;
    let arr = resp["whirlpools"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for p in arr {
        let tvl = p["tvl"].as_f64().unwrap_or(0.0);
        // v1 `volume` may be a number or an object with a `day` field.
        let volume = p["volume"]
            .as_f64()
            .or_else(|| p["volume"]["day"].as_f64())
            .unwrap_or(0.0);
        if tvl < min_tvl || (max_tvl > 0.0 && tvl > max_tvl) || volume < min_vol {
            continue;
        }
        let (Some(addr), Some(a), Some(b)) = (
            p["address"].as_str(),
            p["tokenA"]["mint"].as_str(),
            p["tokenB"]["mint"].as_str(),
        ) else {
            continue;
        };
        out.push(Candidate {
            address: addr.to_string(),
            dex: "orca-whirlpool",
            mint_a: a.to_string(),
            mint_b: b.to_string(),
            tvl,
            volume,
            label: pair_label("orca-whirlpool", a, b),
        });
    }
    Ok(out)
}

/// token -> count of pools touching it, over a candidate slice.
fn token_degrees(pools: &[Candidate]) -> HashMap<String, usize> {
    let mut deg: HashMap<String, usize> = HashMap::new();
    for p in pools {
        *deg.entry(p.mint_a.clone()).or_default() += 1;
        *deg.entry(p.mint_b.clone()).or_default() += 1;
    }
    deg
}

/// Iteratively drop pools whose either token has degree < 2 (can never be on a
/// cycle). Repeats to a fixpoint, since dropping a pool lowers other degrees.
fn prune_degree(mut pools: Vec<Candidate>) -> Vec<Candidate> {
    loop {
        let deg = token_degrees(&pools);
        let before = pools.len();
        pools.retain(|p| deg[&p.mint_a] >= 2 && deg[&p.mint_b] >= 2);
        if pools.len() == before {
            return pools;
        }
    }
}

/// Keep only pools in the connected component(s) reachable from a base mint.
fn keep_base_connected(pools: Vec<Candidate>, bases: &HashSet<String>) -> Vec<Candidate> {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for p in &pools {
        adj.entry(&p.mint_a).or_default().push(&p.mint_b);
        adj.entry(&p.mint_b).or_default().push(&p.mint_a);
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = bases
        .iter()
        .filter(|b| adj.contains_key(b.as_str()))
        .cloned()
        .collect();
    while let Some(t) = stack.pop() {
        if !seen.insert(t.clone()) {
            continue;
        }
        for n in adj.get(t.as_str()).cloned().unwrap_or_default() {
            if !seen.contains(n) {
                stack.push(n.to_string());
            }
        }
    }
    pools
        .into_iter()
        .filter(|p| seen.contains(&p.mint_a) && seen.contains(&p.mint_b))
        .collect()
}

/// Other endpoint of pool `i` given one of its mints.
fn other_mint<'a>(pools: &'a [Candidate], i: usize, m: &str) -> &'a str {
    if pools[i].mint_a == m {
        &pools[i].mint_b
    } else {
        &pools[i].mint_a
    }
}

#[allow(clippy::too_many_arguments)]
fn count_dfs(
    base: &str,
    cur: &str,
    pool_path: &mut Vec<usize>,
    mint_path: &mut Vec<String>,
    adj: &HashMap<String, Vec<usize>>,
    pools: &[Candidate],
    max_hops: usize,
    seen: &mut HashSet<String>,
) {
    let neighbors = adj.get(cur).cloned().unwrap_or_default();
    for pi in neighbors {
        if pool_path.contains(&pi) {
            continue;
        }
        let next = other_mint(pools, pi, cur).to_string();
        if next == base {
            if !pool_path.is_empty() {
                let mut key: Vec<String> = pool_path
                    .iter()
                    .map(|&i| pools[i].address.clone())
                    .collect();
                key.push(pools[pi].address.clone());
                seen.insert(key.join(">"));
            }
            continue;
        }
        if pool_path.len() + 1 >= max_hops || mint_path.contains(&next) {
            continue;
        }
        pool_path.push(pi);
        mint_path.push(next.clone());
        count_dfs(
            base, &next, pool_path, mint_path, adj, pools, max_hops, seen,
        );
        pool_path.pop();
        mint_path.pop();
    }
}

/// Count simple cycles (<= max_hops, no repeated pool/intermediate mint)
/// anchored at each base mint — mirrors DiscoveryEngine's enumeration.
fn count_cycles(pools: &[Candidate], bases: &HashSet<String>, max_hops: usize) -> usize {
    let mut adj: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in pools.iter().enumerate() {
        adj.entry(p.mint_a.clone()).or_default().push(i);
        adj.entry(p.mint_b.clone()).or_default().push(i);
    }
    let mut seen: HashSet<String> = HashSet::new();
    for base in bases {
        if adj.contains_key(base) {
            count_dfs(
                base,
                base,
                &mut Vec::new(),
                &mut Vec::new(),
                &adj,
                pools,
                max_hops,
                &mut seen,
            );
        }
    }
    seen.len()
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt().with_env_filter("info").init();

    let min_tvl = env_f64("DISCOVER_MIN_TVL_USD", 25_000.0);
    let max_tvl = env_f64("DISCOVER_MAX_TVL_USD", 0.0);
    let min_vol = env_f64("DISCOVER_MIN_VOL24H_USD", 5_000.0);
    let max_pools = env_usize("DISCOVER_MAX_POOLS", 100);
    let max_hops = env_usize("DISCOVER_MAX_HOPS", 3).clamp(2, 4);
    let ray_pages = env_usize("RAYDIUM_PAGES", 6);
    let out_file = env_str("POOLS_OUT", "pools.generated.json");

    let bases: HashSet<String> = env_str("BASE_MINTS", WSOL)
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    println!(
        "discover-pools: min_tvl=${min_tvl:.0} max_tvl={} min_vol24h=${min_vol:.0} max_pools={max_pools} max_hops={max_hops}",
        if max_tvl > 0.0 { format!("${max_tvl:.0}") } else { "none".into() }
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut candidates = fetch_raydium(&client, ray_pages, min_tvl, max_tvl, min_vol)
        .await
        .unwrap_or_else(|e| {
            eprintln!("raydium fetch failed: {e}");
            Vec::new()
        });
    let ray_n = candidates.len();
    let orca = fetch_orca(&client, min_tvl, max_tvl, min_vol)
        .await
        .unwrap_or_else(|e| {
            eprintln!("orca fetch failed: {e}");
            Vec::new()
        });
    let orca_n = orca.len();
    candidates.extend(orca);

    // Dedup by address (some pools appear across paginated fetches).
    let mut seen_addr = HashSet::new();
    candidates.retain(|c| seen_addr.insert(c.address.clone()));
    println!(
        "fetched: {ray_n} raydium + {orca_n} orca = {} candidates after tvl/vol filter",
        candidates.len()
    );

    // Keep only cycle-capable pools: degree>=2 fixpoint, then base-connected.
    candidates = prune_degree(candidates);
    candidates = keep_base_connected(candidates, &bases);
    candidates = prune_degree(candidates); // base-connect may have removed pools
    println!(
        "after cycle-pruning (degree>=2 + base-connected): {} pools",
        candidates.len()
    );

    // Rank by 24h volume (activity), cap, then re-prune to keep the graph
    // cycle-valid after the cut.
    candidates.sort_by(|a, b| {
        b.volume
            .partial_cmp(&a.volume)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(max_pools);
    candidates = prune_degree(candidates);
    candidates = keep_base_connected(candidates, &bases);
    candidates = prune_degree(candidates);

    if candidates.is_empty() {
        eprintln!("no cycle-capable pools survived the filters — loosen DISCOVER_MIN_TVL_USD / DISCOVER_MIN_VOL24H_USD");
        std::process::exit(1);
    }

    // ── Write pools.generated.json (schema matches pools.json) ───────────────
    let pools_json: Vec<Value> = candidates
        .iter()
        .map(|c| json!({ "label": c.label, "dex": c.dex, "address": c.address }))
        .collect();
    let doc = json!({
        "comment": "Auto-generated by discover-pools. Cycle-capable Raydium v4 + Orca pools. Vaults/mints/fees are hydrated from chain at bootstrap.",
        "pools": pools_json,
    });
    std::fs::write(&out_file, serde_json::to_string_pretty(&doc)? + "\n")
        .with_context(|| format!("write {out_file}"))?;

    // ── Summary ──────────────────────────────────────────────────────────────
    let routes = count_cycles(&candidates, &bases, max_hops);
    let deg = token_degrees(&candidates);
    let unique_tokens = deg.len();
    let ray_final = candidates.iter().filter(|c| c.dex == "raydium-v4").count();
    let orca_final = candidates.len() - ray_final;
    let mut top: Vec<(&String, &usize)> = deg.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));

    let mut tvls: Vec<f64> = candidates.iter().map(|c| c.tvl).collect();
    tvls.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let total_tvl: f64 = tvls.iter().sum();
    let median_tvl = tvls.get(tvls.len() / 2).copied().unwrap_or(0.0);

    println!("\n══════════ pools.generated.json ══════════");
    println!("  pools:          {}", candidates.len());
    println!("  unique tokens:  {unique_tokens}");
    println!(
        "  liquidity:      ${:.0}M total, ${:.0}k median, range ${:.0}k–${:.0}M",
        total_tvl / 1e6,
        median_tvl / 1e3,
        tvls.first().copied().unwrap_or(0.0) / 1e3,
        tvls.last().copied().unwrap_or(0.0) / 1e6
    );
    println!(
        "  cycle routes:   {routes}  (<= {max_hops} hops, base = {})",
        bases
            .iter()
            .map(|b| symbol(b))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    );
    println!("  dex breakdown:  raydium-v4={ray_final}  orca-whirlpool={orca_final}");
    println!("  top connected tokens:");
    for (mint, d) in top.iter().take(8) {
        let s = symbol(mint);
        let name = if s.is_empty() { &mint[..8] } else { s };
        println!("      {name:<8} in {d} pools");
    }
    println!("\n  wrote {out_file}  (pools.json left untouched)");
    println!("  next: POOLS_FILE={out_file} cargo run -p arb-monitor --bin preview");
    Ok(())
}
