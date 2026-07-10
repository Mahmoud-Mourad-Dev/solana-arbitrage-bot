//! Rust side of the differential-parity harness. Loads
//! validation/scenarios.json (the SAME file the TS harness reads), runs the
//! production `DiscoveryEngine`, and writes normalized opportunities to
//! validation/out-rs.json for byte-comparison against out-ts.json.
//!
//! Run: cargo run -p arb-monitor --bin parity_harness

use arb_monitor::config::MonitorConfig;
use arb_monitor::discovery::DiscoveryEngine;
use arb_monitor::parsers::{TickInfo, TICKS_PER_ARRAY};
use arb_monitor::registry::PoolRegistry;
use arb_monitor::types::{
    tick_array_starts_around, PoolCommon, PoolState, RaydiumPool, WhirlpoolPool,
};
use serde_json::{json, Map, Value};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::str::FromStr;

/// Approximate tick index for a Q64.64 sqrt price (harness only).
fn tick_from_sqrt(sqrt_price: u128) -> i32 {
    let ratio = sqrt_price as f64 / (1u128 << 64) as f64;
    (2.0 * ratio.ln() / 1.0001_f64.ln()).round() as i32
}

/// 5 empty tick arrays around `tick` — deep uniform liquidity, no crossings.
fn uniform_tick_arrays(tick: i32, spacing: u16) -> HashMap<i32, Vec<TickInfo>> {
    let empty = vec![
        TickInfo {
            initialized: false,
            liquidity_net: 0
        };
        TICKS_PER_ARRAY
    ];
    tick_array_starts_around(tick, spacing)
        .into_iter()
        .map(|s| (s, empty.clone()))
        .collect()
}

struct Fixtures {
    mints: HashMap<String, String>,
    addresses: HashMap<String, String>,
    root: Value,
}

impl Fixtures {
    fn sym(map: &HashMap<String, String>, k: &str) -> String {
        map.get(k).cloned().unwrap_or_else(|| k.to_string())
    }
    fn mint(&self, k: &str) -> Pubkey {
        Pubkey::from_str(&Self::sym(&self.mints, k)).expect("valid mint")
    }
    fn addr(&self, k: &str) -> Pubkey {
        Pubkey::from_str(&Self::sym(&self.addresses, k)).expect("valid address")
    }
}

fn u64_field(v: &Value, k: &str) -> u64 {
    match &v[k] {
        Value::String(s) => s.parse().unwrap(),
        Value::Number(n) => n.as_u64().unwrap(),
        _ => panic!("missing u64 field {k}"),
    }
}

fn u128_field(v: &Value, k: &str) -> u128 {
    v[k].as_str().unwrap().parse().unwrap()
}

fn build_config(f: &Fixtures) -> MonitorConfig {
    let c = &f.root["config"];
    let mut trade_bounds = HashMap::new();
    for (mint_sym, b) in c["tradeBounds"].as_object().unwrap() {
        trade_bounds.insert(
            f.mint(mint_sym),
            (
                b["min"].as_str().unwrap().parse().unwrap(),
                b["max"].as_str().unwrap().parse().unwrap(),
            ),
        );
    }
    MonitorConfig {
        geyser_endpoint: String::new(),
        geyser_x_token: None,
        rpc_endpoint: String::new(),
        redis_url: String::new(),
        opportunity_channel: String::new(),
        opportunity_list: String::new(),
        opportunity_list_max: 1000,
        base_mints: c["baseMints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| f.mint(m.as_str().unwrap()))
            .collect(),
        max_hops: c["maxHops"].as_u64().unwrap() as usize,
        min_profit_bps: c["minProfitBps"].as_u64().unwrap(),
        slippage_bps: c["slippageBps"].as_u64().unwrap(),
        max_clmm_impact_bps: c["maxClmmImpactBps"].as_u64().unwrap(),
        trade_bounds,
        base_signature_fee_lamports: u64_field(c, "baseSignatureFeeLamports"),
        priority_fee_lamports: u64_field(c, "priorityFeeLamports"),
        jito_tip_lamports: u64_field(c, "jitoTipLamports"),
        opportunity_cooldown_ms: c["opportunityCooldownMs"].as_u64().unwrap(),
        pools: vec![],
    }
}

fn build_pool(f: &Fixtures, p: &Value) -> PoolState {
    let common = PoolCommon {
        address: f.addr(p["address"].as_str().unwrap()),
        label: p["address"].as_str().map(str::to_string),
        mint_a: f.mint(p["mintA"].as_str().unwrap()),
        mint_b: f.mint(p["mintB"].as_str().unwrap()),
        vault_a: f.addr(p["vaultA"].as_str().unwrap()),
        vault_b: f.addr(p["vaultB"].as_str().unwrap()),
        decimals_a: p["decimalsA"].as_u64().unwrap() as u8,
        decimals_b: p["decimalsB"].as_u64().unwrap() as u8,
        last_slot: p["slot"].as_u64().unwrap(),
        last_updated_ms: 1,
        ready: true,
    };
    if p["dex"] == "raydium-v4" {
        PoolState::Raydium(RaydiumPool {
            common,
            vault_a_balance: u64_field(p, "reserveBase"),
            vault_b_balance: u64_field(p, "reserveQuote"),
            open_orders: f.addr(p["openOrders"].as_str().unwrap()),
            open_orders_base_total: 0,
            open_orders_quote_total: 0,
            base_need_take_pnl: 0,
            quote_need_take_pnl: 0,
            swap_fee_numerator: p["feeNum"].as_u64().unwrap(),
            swap_fee_denominator: p["feeDen"].as_u64().unwrap(),
            status: p["status"].as_u64().unwrap(),
            pool_open_time: u64_field(p, "poolOpenTime"),
        })
    } else {
        let sqrt = u128_field(p, "sqrtPriceX64");
        let tick = tick_from_sqrt(sqrt);
        PoolState::Whirlpool(WhirlpoolPool {
            common,
            sqrt_price_x64: sqrt,
            liquidity: u128_field(p, "liquidity"),
            tick_current_index: tick,
            tick_spacing: 64,
            fee_rate_ppm: p["feeRatePpm"].as_u64().unwrap(),
            // Deep uniform liquidity: 5 empty tick arrays around the current
            // tick → no crossings within the traded range → the exact quote
            // reduces to the single-tick closed form, matching the TS engine.
            tick_arrays: uniform_tick_arrays(tick, 64),
        })
    }
}

/// Normalized cycle (discoveredAtMs excluded), matching the TS harness.
fn normalize(opp: &arb_common::opportunity::Opportunity) -> Value {
    let hops: Vec<Value> = opp
        .hops
        .iter()
        .map(|h| {
            json!({
                "pool": h.pool,
                "dex": match h.dex {
                    arb_common::ix::DexKind::RaydiumV4 => "raydium-v4",
                    arb_common::ix::DexKind::OrcaWhirlpool => "orca-whirlpool",
                },
                "inputMint": h.input_mint,
                "outputMint": h.output_mint,
                "amountIn": h.amount_in.to_string(),
                "expectedAmountOut": h.expected_amount_out.to_string(),
                "minAmountOut": h.min_amount_out.to_string(),
            })
        })
        .collect();
    json!({
        "id": opp.id,
        "baseMint": opp.base_mint,
        "baseSymbol": opp.base_symbol.clone().map(Value::String).unwrap_or(Value::Null),
        "amountIn": opp.amount_in.to_string(),
        "expectedAmountOut": opp.expected_amount_out.to_string(),
        "grossProfit": opp.gross_profit.to_string(),
        "estimatedCostInBase": opp.estimated_cost_in_base.to_string(),
        "netProfit": opp.net_profit.to_string(),
        "netProfitBps": opp.net_profit_bps,
        "slot": opp.slot.to_string(),
        "hops": hops,
    })
}

fn run_scenario(f: &Fixtures, scenario: &Value) -> Vec<Value> {
    let cfg = build_config(f);
    let mut registry = PoolRegistry::new();
    for p in scenario["pools"].as_array().unwrap() {
        let pool = build_pool(f, p);
        let (ma, da, mb, db) = {
            let c = pool.common();
            (c.mint_a, c.decimals_a, c.mint_b, c.decimals_b)
        };
        registry.register_token(ma, da);
        registry.register_token(mb, db);
        registry.add_pool(pool);
    }
    let mut engine = DiscoveryEngine::new(cfg.opportunity_cooldown_ms);
    engine.build_cycle_index(&registry, &cfg);
    for d in scenario["dirty"].as_array().unwrap() {
        engine.mark_dirty(f.addr(d.as_str().unwrap()));
    }
    let mut opps: Vec<Value> = engine
        .run_search(&registry, &cfg, None)
        .iter()
        .map(normalize)
        .collect();
    opps.sort_by(|a, b| a["id"].as_str().unwrap().cmp(b["id"].as_str().unwrap()));
    opps
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "validation/scenarios.json".to_string());
    let root: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let fixtures = Fixtures {
        mints: serde_json::from_value(root["mints"].clone())?,
        addresses: serde_json::from_value(root["addresses"].clone())?,
        root: root.clone(),
    };

    let mut out = Map::new();
    let mut total = 0usize;
    for scenario in root["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap().to_string();
        let opps = run_scenario(&fixtures, scenario);
        total += opps.len();
        out.insert(name, Value::Array(opps));
    }

    let serialized = serde_json::to_string_pretty(&Value::Object(out))?;
    std::fs::write("validation/out-rs.json", serialized + "\n")?;
    println!(
        "parity_harness: {} scenarios, {total} opportunities -> out-rs.json",
        root["scenarios"].as_array().unwrap().len()
    );
    Ok(())
}
