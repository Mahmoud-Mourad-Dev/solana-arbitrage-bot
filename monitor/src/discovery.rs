//! Cycle discovery engine — port of the `DiscoveryEngine` half of
//! `src/graph.ts`. All cycle routes (<= max_hops) are enumerated ONCE at
//! startup and indexed by pool; a dirty pool re-simulates only the routes
//! touching it. Unlike the TS version this is synchronous (called from the
//! async Geyser task), so there is no setImmediate chunking — a single
//! evaluation pass is bounded by the (small, static) route set.

use crate::config::MonitorConfig;
use crate::math::optimize_input;
use crate::quote::quote_pool;
use crate::registry::{now_ms, PoolRegistry};
use arb_common::opportunity::{Opportunity, OpportunityHop};
use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;
use std::collections::{HashMap, HashSet};

pub const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";

/// Static route (pool sequence anchored at a base mint).
#[derive(Debug, Clone)]
pub struct CycleRoute {
    pub key: String,
    pub base_mint: Pubkey,
    pub pools: Vec<Pubkey>,
    /// input mint per hop (output of hop i == input of hop i+1).
    pub input_mints: Vec<Pubkey>,
}

#[derive(Debug, Default, Clone)]
pub struct EngineStats {
    pub searches: u64,
    pub routes_evaluated: u64,
    pub opportunities: u64,
    pub suppressed_by_cooldown: u64,
}

pub struct DiscoveryEngine {
    routes: Vec<CycleRoute>,
    routes_by_pool: HashMap<Pubkey, Vec<usize>>,
    dirty: HashSet<Pubkey>,
    last_published: HashMap<String, (u64, i128)>,
    pub stats: EngineStats,
    wsol: Pubkey,
    cooldown_ms: u64,
}

impl DiscoveryEngine {
    pub fn new(cooldown_ms: u64) -> Self {
        Self {
            routes: Vec::new(),
            routes_by_pool: HashMap::new(),
            dirty: HashSet::new(),
            last_published: HashMap::new(),
            stats: EngineStats::default(),
            wsol: WSOL_MINT_STR.parse().unwrap(),
            cooldown_ms,
        }
    }

    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    /// Enumerate every base-anchored cycle of length 2..=max_hops.
    pub fn build_cycle_index(&mut self, registry: &PoolRegistry, cfg: &MonitorConfig) {
        let mut seen = HashSet::new();
        for base in &cfg.base_mints {
            if !registry.adjacency.contains_key(base) {
                continue;
            }
            self.dfs(
                registry,
                cfg,
                *base,
                *base,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut seen,
            );
        }
        for (idx, route) in self.routes.iter().enumerate() {
            for pool in &route.pools {
                self.routes_by_pool.entry(*pool).or_default().push(idx);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dfs(
        &mut self,
        registry: &PoolRegistry,
        cfg: &MonitorConfig,
        base: Pubkey,
        current_mint: Pubkey,
        pool_path: &mut Vec<Pubkey>,
        mint_path: &mut Vec<Pubkey>,
        seen: &mut HashSet<String>,
    ) {
        let Some(neighbors) = registry.adjacency.get(&current_mint) else {
            return;
        };
        for pool_addr in neighbors.clone() {
            if pool_path.contains(&pool_addr) {
                continue; // never reuse a pool
            }
            let Some(pool) = registry.pools.get(&pool_addr) else {
                continue;
            };
            let next = pool.other_mint(&current_mint);

            if next == base {
                if !pool_path.is_empty() {
                    let mut pools = pool_path.clone();
                    pools.push(pool_addr);
                    let key = route_key(&pools);
                    if seen.insert(key.clone()) {
                        let mut input_mints = vec![base];
                        input_mints.extend_from_slice(mint_path);
                        self.routes.push(CycleRoute {
                            key,
                            base_mint: base,
                            pools,
                            input_mints,
                        });
                    }
                }
                continue;
            }

            if pool_path.len() + 1 >= cfg.max_hops {
                continue;
            }
            if mint_path.contains(&next) {
                continue;
            }
            pool_path.push(pool_addr);
            mint_path.push(next);
            self.dfs(registry, cfg, base, next, pool_path, mint_path, seen);
            pool_path.pop();
            mint_path.pop();
        }
    }

    /// Hot-path entry: mark a pool dirty. Returns true if it's on a route.
    pub fn mark_dirty(&mut self, pool: Pubkey) -> bool {
        if self.routes_by_pool.contains_key(&pool) {
            self.dirty.insert(pool);
            true
        } else {
            false
        }
    }

    /// Evaluate all routes touched by the dirty set; emit opportunities.
    pub fn run_search(&mut self, registry: &PoolRegistry, cfg: &MonitorConfig) -> Vec<Opportunity> {
        if self.dirty.is_empty() {
            return Vec::new();
        }
        self.stats.searches += 1;
        let mut candidates: HashSet<usize> = HashSet::new();
        for pool in self.dirty.drain() {
            if let Some(idxs) = self.routes_by_pool.get(&pool) {
                candidates.extend(idxs);
            }
        }

        let mut out = Vec::new();
        for idx in candidates {
            self.stats.routes_evaluated += 1;
            let route = self.routes[idx].clone();
            if let Some(opp) = self.evaluate_route(&route, registry, cfg) {
                if self.should_publish(&route, &opp) {
                    self.stats.opportunities += 1;
                    out.push(opp);
                }
            }
        }
        out
    }

    fn evaluate_route(
        &self,
        route: &CycleRoute,
        registry: &PoolRegistry,
        cfg: &MonitorConfig,
    ) -> Option<Opportunity> {
        let now_sec = now_ms() / 1000;
        let mut max_slot = 0u64;
        let mut pools = Vec::with_capacity(route.pools.len());
        for addr in &route.pools {
            let p = registry.pools.get(addr)?;
            if !p.common().ready || !p.swap_enabled(now_sec) {
                return None;
            }
            max_slot = max_slot.max(p.common().last_slot);
            pools.push(p);
        }

        let bounds = cfg.trade_bounds.get(&route.base_mint)?;
        let simulate = |amount_in: u64| -> u64 {
            let mut amount = amount_in;
            for (h, p) in pools.iter().enumerate() {
                amount = quote_pool(p, &route.input_mints[h], amount, cfg.max_clmm_impact_bps);
                if amount == 0 {
                    return 0;
                }
            }
            amount
        };

        let (amount_in, gross_profit) =
            optimize_input(|x| simulate(x) as i128 - x as i128, bounds.0, bounds.1, 48);
        if gross_profit <= 0 || amount_in == 0 {
            return None;
        }
        let gross_profit = gross_profit as u64;

        let cost = self.execution_cost_in_base(&route.base_mint, registry, cfg)?;
        let net_profit = gross_profit.checked_sub(cost)?;
        if net_profit == 0 {
            return None;
        }
        let net_profit_bps = (net_profit as u128 * 10_000 / amount_in as u128) as u64;
        if net_profit_bps < cfg.min_profit_bps {
            return None;
        }

        // Re-walk to capture exact per-hop legs.
        let mut hops = Vec::with_capacity(pools.len());
        let mut amount = amount_in;
        for (h, p) in pools.iter().enumerate() {
            let input_mint = route.input_mints[h];
            let out = quote_pool(p, &input_mint, amount, cfg.max_clmm_impact_bps);
            if out == 0 {
                return None;
            }
            let output_mint = p.other_mint(&input_mint);
            hops.push(OpportunityHop {
                pool: p.common().address.to_string(),
                dex: p.dex(),
                input_mint: input_mint.to_string(),
                output_mint: output_mint.to_string(),
                amount_in: amount,
                expected_amount_out: out,
                min_amount_out: (out as u128 * (10_000 - cfg.slippage_bps as u128) / 10_000) as u64,
            });
            amount = out;
        }

        let base_symbol = registry
            .tokens
            .get(&route.base_mint)
            .and_then(|t| t.symbol)
            .map(str::to_string);

        Some(Opportunity {
            id: short_hash(&route.key),
            base_mint: route.base_mint.to_string(),
            base_symbol,
            hops,
            amount_in,
            expected_amount_out: amount,
            gross_profit: amount - amount_in,
            estimated_cost_in_base: cost,
            net_profit: amount - amount_in - cost,
            net_profit_bps: net_profit_bps as f64,
            slot: max_slot,
            discovered_at_ms: now_ms(),
        })
    }

    /// Execution cost (sig + priority + tip) in the base mint. WSOL: direct
    /// lamports. Otherwise price through the freshest WSOL/base pool; None
    /// if unpriceable (cycle skipped, never published with unpriced cost).
    fn execution_cost_in_base(
        &self,
        base_mint: &Pubkey,
        registry: &PoolRegistry,
        cfg: &MonitorConfig,
    ) -> Option<u64> {
        let lamports =
            cfg.base_signature_fee_lamports + cfg.priority_fee_lamports + cfg.jito_tip_lamports;
        if base_mint == &self.wsol {
            return Some(lamports);
        }
        let refp = registry.find_reference_pool(&self.wsol, base_mint)?;
        let converted = quote_pool(refp, &self.wsol, lamports, cfg.max_clmm_impact_bps);
        (converted > 0).then_some(converted)
    }

    fn should_publish(&mut self, route: &CycleRoute, opp: &Opportunity) -> bool {
        let now = opp.discovered_at_ms;
        if let Some((at, profit)) = self.last_published.get(&route.key) {
            if now - at < self.cooldown_ms {
                // within cooldown: only a materially better (>5%) quote.
                if (opp.net_profit as i128) <= profit * 105 / 100 {
                    self.stats.suppressed_by_cooldown += 1;
                    return false;
                }
            }
        }
        self.last_published
            .insert(route.key.clone(), (now, opp.net_profit as i128));
        true
    }
}

fn route_key(pools: &[Pubkey]) -> String {
    pools
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(">")
}

/// Short id — first 16 hex chars of SHA-256(route key). Matches the TS
/// `shortHash` so the SAME cycle carries the SAME id across both monitors.
pub fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex_lower(&digest[..8])
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// short_hash must match the TS shortHash (sha256 hex, first 16 chars)
    /// so a given cycle carries the SAME id from either monitor. References
    /// captured from the compiled TS util (dist/utils.js).
    #[test]
    fn short_hash_matches_typescript() {
        assert_eq!(short_hash("a>b>c"), "3b65cc6c2f692613");
        assert_eq!(short_hash("RAY_POOL>ORCA_POOL"), "d1abf0e5a0cc8eed");
    }

    #[test]
    fn route_key_joins_with_gt() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        assert_eq!(route_key(&[a, b]), format!("{a}>{b}"));
    }

    /// End-to-end parity with the TS `npm run selftest`: two SOL/USDC venues
    /// with a deliberate ~2% price gap (Raydium 150, Orca ~153). The engine
    /// must find the cycle, pick the profitable direction (buy USDC where SOL
    /// is dear on Orca, buy SOL back where cheap on Raydium), 2 hops, net>0.
    #[test]
    fn discovers_two_percent_discrepancy_like_ts() {
        use crate::config::MonitorConfig;
        use crate::types::{PoolCommon, PoolState, RaydiumPool, WhirlpoolPool};
        use std::collections::HashMap;

        let wsol: Pubkey = WSOL_MINT_STR.parse().unwrap();
        let usdc: Pubkey = crate::config::USDC_MINT_STR.parse().unwrap();
        let ray_addr = Pubkey::new_unique();
        let orca_addr = Pubkey::new_unique();

        let mut reg = PoolRegistry::new();
        reg.register_token(wsol, 9);
        reg.register_token(usdc, 6);

        // Raydium: 5000 SOL / 750000 USDC -> 150 USDC/SOL.
        reg.add_pool(PoolState::Raydium(RaydiumPool {
            common: PoolCommon {
                address: ray_addr,
                label: None,
                mint_a: wsol,
                mint_b: usdc,
                vault_a: Pubkey::new_unique(),
                vault_b: Pubkey::new_unique(),
                decimals_a: 9,
                decimals_b: 6,
                last_slot: 1,
                last_updated_ms: now_ms(),
                ready: true,
            },
            vault_a_balance: 5_000 * 10u64.pow(9),
            vault_b_balance: 750_000 * 10u64.pow(6),
            open_orders: Pubkey::new_unique(),
            open_orders_base_total: 0,
            open_orders_quote_total: 0,
            base_need_take_pnl: 0,
            quote_need_take_pnl: 0,
            swap_fee_numerator: 25,
            swap_fee_denominator: 10_000,
            status: 6,
            pool_open_time: 0,
        }));

        // Orca: sqrtPrice for ~153 USDC/SOL (same constant as TS selftest).
        reg.add_pool(PoolState::Whirlpool(WhirlpoolPool {
            common: PoolCommon {
                address: orca_addr,
                label: None,
                mint_a: wsol,
                mint_b: usdc,
                vault_a: Pubkey::new_unique(),
                vault_b: Pubkey::new_unique(),
                decimals_a: 9,
                decimals_b: 6,
                last_slot: 1,
                last_updated_ms: now_ms(),
                ready: true,
            },
            sqrt_price_x64: 7_216_072_408_257_405_000,
            liquidity: 10u128.pow(16),
            tick_current_index: 0,
            tick_spacing: 64,
            fee_rate_ppm: 3_000,
        }));

        let mut bounds = HashMap::new();
        bounds.insert(wsol, (50_000_000u64, 10_000_000_000u64));
        let cfg = MonitorConfig {
            geyser_endpoint: String::new(),
            geyser_x_token: None,
            rpc_endpoint: String::new(),
            redis_url: String::new(),
            opportunity_channel: String::new(),
            opportunity_list: String::new(),
            opportunity_list_max: 1000,
            base_mints: vec![wsol],
            max_hops: 4,
            min_profit_bps: 5,
            slippage_bps: 20,
            max_clmm_impact_bps: 100,
            trade_bounds: bounds,
            base_signature_fee_lamports: 5_000,
            priority_fee_lamports: 100_000,
            jito_tip_lamports: 1_000_000,
            opportunity_cooldown_ms: 500,
            pools: vec![],
        };

        let mut engine = DiscoveryEngine::new(cfg.opportunity_cooldown_ms);
        engine.build_cycle_index(&reg, &cfg);
        assert!(engine.route_count() > 0, "no routes built");

        engine.mark_dirty(ray_addr);
        engine.mark_dirty(orca_addr);
        let opps = engine.run_search(&reg, &cfg);

        assert!(!opps.is_empty(), "engine found no cycle in a 2% gap");
        let best = opps.iter().max_by_key(|o| o.net_profit).unwrap();
        assert_eq!(best.base_mint, WSOL_MINT_STR);
        assert_eq!(best.hops.len(), 2);
        assert!(best.net_profit > 0, "non-positive net profit");
        // Profitable direction enters through Orca (SOL dear) first.
        assert_eq!(
            best.hops[0].pool,
            orca_addr.to_string(),
            "picked losing direction"
        );
        assert_eq!(best.hops[0].output_mint, crate::config::USDC_MINT_STR);
        assert_eq!(best.hops[1].output_mint, WSOL_MINT_STR);
        assert_eq!(
            best.hops[1].amount_in, best.hops[0].expected_amount_out,
            "hop chaining broken"
        );
        assert!(
            best.hops[0].min_amount_out < best.hops[0].expected_amount_out,
            "slippage floor missing"
        );
    }
}
