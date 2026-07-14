//! Two-stage integer trade-size optimizer (S7).
//!
//! Objective: maximise projected NET profit (after the shared cost model) over
//! the WSOL input size for a two-leg [`Route`]. Stage 1 is a coarse log-spaced
//! grid; stage 2 ternary-refines around the best few grid points. Everything is
//! integer; no floating point touches a financial quantity.
//!
//! Rejections are handled honestly:
//! - a STRUCTURAL reject (topology / creator-BUY / wrong mint) makes the whole
//!   route unusable at every size → `None`;
//! - a CAPACITY reject (missing DLMM bins / exhausted liquidity) means "too
//!   big" → it caps the search upward instead of being treated as a loss;
//! - only a size whose net clears `cost.required_net_lamports` is returned.

use crate::route_engine::{Candidate, LegReject, Route, RouteReject};
use arb_common::cost::CostModel;
use solana_sdk::pubkey::Pubkey;

/// Search configuration (all sizes in WSOL lamports).
#[derive(Debug, Clone, Copy)]
pub struct SizeGrid {
    /// Smallest input to consider.
    pub min: u64,
    /// Largest input allowed (risk / balance / pool-safety cap — the caller
    /// computes the binding minimum of these).
    pub max: u64,
    /// Number of coarse grid points (log-spaced), ≥ 2.
    pub coarse_points: usize,
    /// Top-K grid points to locally refine.
    pub refine_top_k: usize,
    /// Ternary-refine iterations per kept point.
    pub refine_iters: usize,
}

impl Default for SizeGrid {
    fn default() -> Self {
        SizeGrid {
            min: 10_000_000,     // 0.01 SOL
            max: 20_000_000_000, // 20 SOL
            coarse_points: 28,
            refine_top_k: 3,
            refine_iters: 40,
        }
    }
}

/// What one probe of a size told us.
enum Probe {
    /// A round trip happened; `score` is the objective value (net when
    /// profitable, else the raw round-trip delta so the search can climb).
    Feasible { score: i128 },
    /// Capacity limit hit at this size — search smaller.
    TooBig,
    /// The route is structurally invalid at every size.
    Dead,
}

fn probe(route: &Route, wsol: &Pubkey, cost: &CostModel, amount: u64) -> Probe {
    match route.round_trip(wsol, amount) {
        Ok((_, wsol_out)) => {
            let score = if wsol_out > amount {
                cost.net(wsol_out - amount)
            } else {
                // Loss: keep it comparable/monotonic so the climb has gradient.
                wsol_out as i128 - amount as i128
            };
            Probe::Feasible { score }
        }
        Err(RouteReject::Leg1(LegReject::Dlmm(e))) | Err(RouteReject::Leg2(LegReject::Dlmm(e)))
            if e.is_capacity() =>
        {
            Probe::TooBig
        }
        // Any other leg/topology error is structural — dead at all sizes.
        Err(_) => Probe::Dead,
    }
}

/// Log-spaced grid points in `[min, max]` (inclusive-ish), strictly increasing.
fn log_grid(min: u64, max: u64, n: usize) -> Vec<u64> {
    let n = n.max(2);
    if min >= max {
        return vec![min];
    }
    let lmin = (min.max(1) as f64).ln();
    let lmax = (max as f64).ln();
    let mut out = Vec::with_capacity(n);
    let mut last = 0u64;
    for i in 0..n {
        let t = i as f64 / (n - 1) as f64;
        let v = (lmin + (lmax - lmin) * t).exp().round() as u64;
        let v = v.clamp(min, max);
        if v != last {
            out.push(v);
            last = v;
        }
    }
    out
}

/// Ternary search for the argmax of `score` on the integer interval `[lo, hi]`,
/// stopping at capacity ceilings. `score(a)` is `None` when the size is TooBig.
fn ternary_max(
    route: &Route,
    wsol: &Pubkey,
    cost: &CostModel,
    mut lo: u64,
    mut hi: u64,
    iters: usize,
) -> Option<(u64, i128)> {
    // Pull `hi` below any capacity ceiling first.
    let feasible_score = |a: u64| match probe(route, wsol, cost, a) {
        Probe::Feasible { score } => Some(score),
        _ => None,
    };
    // If hi is TooBig, binary-shrink it to the largest feasible size.
    if feasible_score(hi).is_none() {
        let (mut good, mut bad) = (lo, hi);
        feasible_score(good)?; // lo itself must be feasible, else give up
        for _ in 0..40 {
            if bad - good <= 1 {
                break;
            }
            let mid = good + (bad - good) / 2;
            if feasible_score(mid).is_some() {
                good = mid;
            } else {
                bad = mid;
            }
        }
        hi = good;
    }
    let mut best = (lo, feasible_score(lo)?);
    let hs = feasible_score(hi)?;
    if hs > best.1 {
        best = (hi, hs);
    }
    for _ in 0..iters {
        if hi.saturating_sub(lo) < 3 {
            break;
        }
        let m1 = lo + (hi - lo) / 3;
        let m2 = hi - (hi - lo) / 3;
        let s1 = feasible_score(m1).unwrap_or(i128::MIN);
        let s2 = feasible_score(m2).unwrap_or(i128::MIN);
        for (a, s) in [(m1, s1), (m2, s2)] {
            if s > best.1 {
                best = (a, s);
            }
        }
        if s1 < s2 {
            lo = m1;
        } else {
            hi = m2;
        }
    }
    // Sweep the final tiny window exactly.
    for a in lo..=hi {
        if let Some(s) = feasible_score(a) {
            if s > best.1 {
                best = (a, s);
            }
        }
    }
    Some(best)
}

/// Find the size that maximises net profit and return the resulting
/// [`Candidate`] — or `None` if the route is structurally dead or no size
/// clears the required-net floor.
pub fn optimize(
    route: &Route,
    wsol: &Pubkey,
    cost: &CostModel,
    grid: &SizeGrid,
) -> Option<Candidate> {
    // Stage 1: coarse grid. Bail immediately on a structural death.
    let points = log_grid(grid.min, grid.max, grid.coarse_points);
    let mut scored: Vec<(usize, u64, i128)> = Vec::new();
    for (i, &a) in points.iter().enumerate() {
        match probe(route, wsol, cost, a) {
            Probe::Feasible { score } => scored.push((i, a, score)),
            Probe::TooBig => {}
            Probe::Dead => return None,
        }
    }
    if scored.is_empty() {
        return None;
    }
    // Keep the best-K grid points by score.
    scored.sort_by_key(|x| std::cmp::Reverse(x.2));
    scored.truncate(grid.refine_top_k.max(1));

    // Stage 2: ternary-refine each kept point within its neighbour bracket.
    let mut best_amount = scored[0].1;
    let mut best_score = i128::MIN;
    for &(idx, amount, score) in &scored {
        if score > best_score {
            best_score = score;
            best_amount = amount;
        }
        let lo = if idx > 0 { points[idx - 1] } else { grid.min };
        let hi = if idx + 1 < points.len() {
            points[idx + 1]
        } else {
            grid.max
        };
        if let Some((a, s)) = ternary_max(route, wsol, cost, lo, hi, grid.refine_iters) {
            if s > best_score {
                best_score = s;
                best_amount = a;
            }
        }
    }

    // Only return a real, profitable, gated Candidate.
    route.evaluate(wsol, best_amount, cost).ok()
}

// ─────────────────────── size analysis (optimizer correction) ──────────────
// Exposes the raw gross-vs-size curve and the net-optimal size under EACH cost
// scenario, so a report can show that an "optimal" size is a real market
// optimum and not an artifact of a stepped Jito-tip schedule.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Jito-tip tier thresholds (gross lamports) where the tip % steps up. An
/// optimizer will happily park a trade just below one of these.
const TIP_TIER_THRESHOLDS: [u64; 3] = [
    1_000_000_000 / 200, // 0.005 SOL: 50%→60%
    1_000_000_000 / 20,  // 0.05  SOL: 60%→70%
    1_000_000_000 / 2,   // 0.5   SOL: 70%→80%
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeAnalysis {
    /// (size_lamports, gross_lamports) over a fixed log grid. gross may be < 0.
    pub curve: Vec<(u64, i128)>,
    pub gross_opt_size_lamports: u64,
    pub gross_opt_lamports: i128,
    /// scenario label → (net-optimal size, net at that size).
    pub net_opt_by_scenario: BTreeMap<String, (u64, i128)>,
    /// True if the competitive net-optimal gross sits within 0.5% just BELOW a
    /// tip-tier threshold — i.e. the size is shaped by the fee schedule.
    pub tip_tier_shaped: bool,
}

/// Sample the gross curve and find gross- and net-optimal sizes. `scenarios`
/// are (label, model). Returns None if the route is structurally dead.
pub fn size_analysis(
    route: &Route,
    wsol: &Pubkey,
    grid: &SizeGrid,
    scenarios: &[(String, CostModel)],
) -> Option<SizeAnalysis> {
    let points = log_grid(grid.min, grid.max, grid.coarse_points.max(24));
    let mut curve: Vec<(u64, i128)> = Vec::new();
    for &a in &points {
        match route.round_trip(wsol, a) {
            Ok((_, out)) => curve.push((a, out as i128 - a as i128)),
            Err(RouteReject::Leg1(LegReject::Dlmm(e)))
            | Err(RouteReject::Leg2(LegReject::Dlmm(e)))
                if e.is_capacity() => {}
            Err(_) => return None, // structural death
        }
    }
    if curve.is_empty() {
        return None;
    }
    let (gross_opt_size, gross_opt) = curve.iter().copied().max_by_key(|&(_, g)| g).unwrap();
    let mut net_opt_by_scenario = BTreeMap::new();
    let mut competitive_gross = None;
    for (label, model) in scenarios {
        let best = curve
            .iter()
            .map(|&(size, gross)| {
                let net = if gross > 0 {
                    model.net(gross as u64)
                } else {
                    gross
                };
                (size, net, gross)
            })
            .max_by_key(|&(_, net, _)| net)
            .unwrap();
        net_opt_by_scenario.insert(label.clone(), (best.0, best.1));
        if label == "competitive" {
            competitive_gross = Some(best.2);
        }
    }
    let tip_tier_shaped = competitive_gross.map(near_tier).unwrap_or(false);
    Some(SizeAnalysis {
        curve,
        gross_opt_size_lamports: gross_opt_size,
        gross_opt_lamports: gross_opt,
        net_opt_by_scenario,
        tip_tier_shaped,
    })
}

/// True if `gross` sits within 0.5% just below a tip-tier threshold.
fn near_tier(gross: i128) -> bool {
    if gross <= 0 {
        return false;
    }
    let g = gross as u64;
    TIP_TIER_THRESHOLDS.iter().any(|&t| {
        let lo = t - t / 200; // 0.5% below
        g >= lo && g < t
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meteora_dlmm::{decode_bin_array, decode_lb_pair};
    use crate::pump_amm::PumpAmmPool;
    use crate::route_engine::Leg;
    use arb_common::cost::ExecutionPayment;
    use std::collections::HashMap;
    use std::str::FromStr;

    const WSOL: &str = "So11111111111111111111111111111111111111112";
    const LB_PAIR_BYTES: &[u8] = include_bytes!("../fixtures/meteora/lbpair_J4cGfY61.bin");
    const BIN_ARRAY_9: &[u8] = include_bytes!("../fixtures/meteora/binarray_idx9_J4cGfY61.bin");

    fn wsol() -> Pubkey {
        Pubkey::from_str(WSOL).unwrap()
    }

    fn pump_leg(bm: Pubkey, qm: Pubkey, br: u64, qr: u64) -> Leg {
        Leg::Pump {
            pool: PumpAmmPool {
                bump: 0,
                index: 0,
                creator: Pubkey::default(),
                base_mint: bm,
                quote_mint: qm,
                lp_mint: Pubkey::default(),
                base_vault: Pubkey::default(),
                quote_vault: Pubkey::default(),
                lp_supply: 0,
                coin_creator: Pubkey::default(),
            },
            base_reserve: br,
            quote_reserve: qr,
        }
    }

    fn cost() -> CostModel {
        CostModel {
            signature_fee_lamports: 5_000,
            required_net_lamports: 0,
            payment: ExecutionPayment::JitoTip {
                min_lamports: 0,
                max_lamports: 100_000_000,
            },
            ..Default::default()
        }
    }

    fn grid() -> SizeGrid {
        SizeGrid {
            min: 10_000_000,
            max: 50_000_000_000,
            coarse_points: 24,
            refine_top_k: 3,
            refine_iters: 40,
        }
    }

    /// Two mispriced pools with slippage on both sides ⇒ an interior optimum.
    fn profitable_route() -> Route {
        let token = Pubkey::new_unique();
        // leg1: WSOL(base)→token(quote), ~11 token per WSOL, moderate depth.
        let leg1 = pump_leg(wsol(), token, 3_000_000_000_000, 33_000_000_000_000);
        // leg2: token(base)→WSOL(quote), pays ~0.1 WSOL per token, deep.
        let leg2 = pump_leg(token, wsol(), 300_000_000_000_000, 33_000_000_000_000);
        Route { leg1, leg2 }
    }

    #[test]
    fn finds_size_at_least_as_good_as_any_grid_point() {
        let route = profitable_route();
        let c = optimize(&route, &wsol(), &cost(), &grid()).expect("a candidate");
        // The optimum's net must dominate every coarse probe.
        for a in log_grid(grid().min, grid().max, grid().coarse_points) {
            if let Ok(other) = route.evaluate(&wsol(), a, &cost()) {
                assert!(
                    c.net_profit >= other.net_profit,
                    "optimizer net {} < grid net {} at {a}",
                    c.net_profit,
                    other.net_profit
                );
            }
        }
        // And it is internally consistent.
        let re = route.evaluate(&wsol(), c.amount_in, &cost()).unwrap();
        assert_eq!(re, c);
        assert!(c.net_profit > 0);
    }

    #[test]
    fn optimizer_beats_a_fine_brute_force_scan() {
        // The strongest optimality check: the two-stage result must be at least
        // as good as the best net over a dense linear scan of the whole range.
        let route = profitable_route();
        let g = grid();
        let cost = cost();
        let c = optimize(&route, &wsol(), &cost, &g).unwrap();
        let mut brute = i128::MIN;
        let steps = 500u64;
        for i in 0..=steps {
            let a = g.min + (g.max - g.min) * i / steps;
            if let Ok(x) = route.evaluate(&wsol(), a, &cost) {
                brute = brute.max(x.net_profit);
            }
        }
        assert!(
            c.net_profit >= brute,
            "optimizer net {} < brute-force net {brute}",
            c.net_profit
        );
    }

    #[test]
    fn structural_death_returns_none() {
        // leg1 WSOL is the QUOTE ⇒ WSOL→token is a creator-pool BUY ⇒ refused.
        let token = Pubkey::new_unique();
        let mut pool = PumpAmmPool {
            bump: 0,
            index: 0,
            creator: Pubkey::default(),
            base_mint: token,
            quote_mint: wsol(),
            lp_mint: Pubkey::default(),
            base_vault: Pubkey::default(),
            quote_vault: Pubkey::default(),
            lp_supply: 0,
            coin_creator: Pubkey::new_unique(),
        };
        pool.coin_creator = Pubkey::new_unique();
        let leg1 = Leg::Pump {
            pool,
            base_reserve: 100_000_000_000_000,
            quote_reserve: 100_000_000_000_000,
        };
        let leg2 = pump_leg(token, wsol(), 100_000_000_000_000, 100_000_000_000_000);
        let route = Route { leg1, leg2 };
        assert!(optimize(&route, &wsol(), &cost(), &grid()).is_none());
    }

    #[test]
    fn no_edge_route_returns_none() {
        let token = Pubkey::new_unique();
        let leg1 = pump_leg(wsol(), token, 1_000_000_000_000, 1_000_000_000_000);
        let leg2 = pump_leg(token, wsol(), 1_000_000_000_000, 1_000_000_000_000);
        let route = Route { leg1, leg2 };
        // Fees make every size a loss ⇒ nothing clears required-net (0 floor
        // still needs net ≥ 0, and net is always < 0 here).
        assert!(optimize(&route, &wsol(), &cost(), &grid()).is_none());
    }

    #[test]
    fn size_analysis_reports_curve_and_flags_tip_tier_boundary() {
        let route = profitable_route();
        let scenarios: Vec<(String, CostModel)> = crate::observe_report::default_scenarios()
            .into_iter()
            .map(|s| (s.label.clone(), s.model()))
            .collect();
        let sa = size_analysis(&route, &wsol(), &grid(), &scenarios).unwrap();
        assert!(!sa.curve.is_empty());
        // gross-opt must dominate every sampled gross.
        for &(_, g) in &sa.curve {
            assert!(sa.gross_opt_lamports >= g);
        }
        assert!(sa.net_opt_by_scenario.contains_key("competitive"));
        // near_tier logic: 0.004999 SOL is just below the 0.005 threshold.
        assert!(near_tier(4_999_000));
        assert!(!near_tier(3_000_000));
        assert!(!near_tier(-5));
    }

    #[test]
    fn size_analysis_none_on_structural_death() {
        // creator-pool BUY leg1 ⇒ dead at all sizes.
        let token = Pubkey::new_unique();
        let mut pool = PumpAmmPool {
            bump: 0,
            index: 0,
            creator: Pubkey::default(),
            base_mint: token,
            quote_mint: wsol(),
            lp_mint: Pubkey::default(),
            base_vault: Pubkey::default(),
            quote_vault: Pubkey::default(),
            lp_supply: 0,
            coin_creator: Pubkey::new_unique(),
        };
        pool.coin_creator = Pubkey::new_unique();
        let route = Route {
            leg1: Leg::Pump {
                pool,
                base_reserve: 1_000_000_000_000,
                quote_reserve: 1_000_000_000_000,
            },
            leg2: pump_leg(token, wsol(), 1_000_000_000_000, 1_000_000_000_000),
        };
        let scenarios: Vec<(String, CostModel)> = crate::observe_report::default_scenarios()
            .into_iter()
            .map(|s| (s.label.clone(), s.model()))
            .collect();
        assert!(size_analysis(&route, &wsol(), &grid(), &scenarios).is_none());
    }

    #[test]
    fn respects_capacity_ceiling_on_real_dlmm_leg() {
        // leg2 is the real DLMM pair holding ONLY array 9: large sizes hit
        // InsufficientBinCoverage. The optimizer must stay under that ceiling
        // and never surface a fabricated (too-big) fill.
        let pair = decode_lb_pair(LB_PAIR_BYTES).unwrap();
        let token = pair.token_x_mint;
        let mut arrays = HashMap::new();
        arrays.insert(9i64, decode_bin_array(BIN_ARRAY_9).unwrap());
        let now = pair.v_parameters.last_update_timestamp + 5;
        let leg1 = pump_leg(wsol(), token, 5_000_000_000_000, 700_000_000_000);
        let leg2 = Leg::Meteora {
            pair,
            arrays,
            now_unix: now,
        };
        let route = Route { leg1, leg2 };
        // Whatever it returns (Some or None), a returned Candidate must
        // round-trip successfully (i.e. its size is within capacity).
        if let Some(c) = optimize(&route, &wsol(), &cost(), &grid()) {
            assert!(route.round_trip(&wsol(), c.amount_in).is_ok());
        }
    }
}
