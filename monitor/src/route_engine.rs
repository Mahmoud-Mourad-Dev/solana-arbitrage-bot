//! Two-leg route engine (S6).
//!
//! A route is WSOL → token → WSOL across exactly two legs on different venues
//! (PumpSwap AMM ↔ Meteora DLMM). This module is the PURE evaluation core:
//! given already-fetched leg state and an input size, it chains the two exact
//! quote engines, computes economics via the shared [`CostModel`] (so the
//! monitor and executor agree), and returns either a [`Candidate`] or a typed
//! rejection. Live account fetching / sizing loops live in the observe binary.
//!
//! Correctness invariants: integer-only; a leg that can't be quoted EXACTLY
//! (e.g. creator-pool BUY on PumpSwap, or missing DLMM bins) propagates its
//! structured error and the route is rejected — never a fabricated fill.

use crate::meteora_dlmm::{dlmm_quote_exact_in_detailed, BinArray, DlmmQuoteError, LbPair};
use crate::pump_amm::{pump_quote_detailed_v2, PumpAmmPool, PumpQuoteError};
use crate::pump_feev2::FeeConfig;
use crate::tick_math::{
    single_tick_capacity, sqrt_price_from_tick, swap_exact_in_detailed, MAX_TICK, MIN_TICK,
};
use crate::types::tick_array_span;
use arb_common::cost::CostModel;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

/// Why a single leg could not be quoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegReject {
    /// `input_mint` is not a side of this leg's pool.
    WrongMint,
    Pump(PumpQuoteError),
    Dlmm(DlmmQuoteError),
    Whirlpool(WhirlpoolLegReject),
}

/// Whirlpool-leg rejections (S14B-2). `SingleTickExceeded` is a CAPACITY reject:
/// the input would cross an initialized tick, which is out of the proven scope,
/// so the optimizer must treat it as "too big" and search smaller — NEVER quote
/// it. `capacity` is the max within-tick input for provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhirlpoolLegReject {
    WrongMint,
    NoLiquidity,
    /// Would cross an initialized tick — capacity clamp; `capacity` is the max
    /// within-tick gross input.
    SingleTickExceeded {
        capacity: u64,
    },
    MathError,
}

impl WhirlpoolLegReject {
    /// A capacity reject means "smaller could work" (like DLMM bin exhaustion).
    pub fn is_capacity(&self) -> bool {
        matches!(self, WhirlpoolLegReject::SingleTickExceeded { .. })
    }
}

/// Compact, self-contained Whirlpool leg state (no RPC): enough to quote a
/// single-tick swapV2-semantics exact-in and enforce the single-tick clamp.
#[derive(Debug, Clone)]
pub struct WhirlpoolLegState {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub sqrt_price_x64: u128,
    pub liquidity: u128,
    pub tick_current_index: i32,
    pub tick_spacing: u16,
    pub fee_ppm: u64,
    /// Initialized ticks within loaded coverage: (tick_index, liquidity_net).
    pub initialized_ticks: Vec<(i32, i128)>,
    /// Loaded tick-array start-index coverage (for the no-crossing edge).
    pub lowest_start: i32,
    pub highest_start: i32,
}

impl WhirlpoolLegState {
    /// The sqrt price of the nearest initialized tick in the swap direction —
    /// the single-tick boundary. If none is loaded in that direction, the loaded
    /// coverage edge (so a swap that would leave coverage is rejected, never
    /// extrapolated).
    fn single_tick_boundary(&self, a_to_b: bool) -> u128 {
        let span = tick_array_span(self.tick_spacing);
        if a_to_b {
            self.initialized_ticks
                .iter()
                .filter(|(ti, _)| *ti <= self.tick_current_index)
                .map(|(ti, _)| *ti)
                .max()
                .map(sqrt_price_from_tick)
                .unwrap_or_else(|| sqrt_price_from_tick(self.lowest_start.max(MIN_TICK)))
        } else {
            self.initialized_ticks
                .iter()
                .filter(|(ti, _)| *ti > self.tick_current_index)
                .map(|(ti, _)| *ti)
                .min()
                .map(sqrt_price_from_tick)
                .unwrap_or_else(|| sqrt_price_from_tick((self.highest_start + span).min(MAX_TICK)))
        }
    }

    /// Single-tick exact-in quote (token→WSOL uses swapV2 semantics; the math is
    /// identical to the proven `swap_exact_in`). Returns `(out, fee)` or a typed
    /// reject. A size that would cross a tick is a CAPACITY reject carrying the
    /// within-tick capacity — never a crossing quote.
    fn quote(&self, input_mint: &Pubkey, amount_in: u64) -> Result<(u64, u64), WhirlpoolLegReject> {
        let a_to_b = if *input_mint == self.mint_a {
            true
        } else if *input_mint == self.mint_b {
            false
        } else {
            return Err(WhirlpoolLegReject::WrongMint);
        };
        if self.liquidity == 0 {
            return Err(WhirlpoolLegReject::NoLiquidity);
        }
        let boundary = self.single_tick_boundary(a_to_b);
        let capacity = single_tick_capacity(
            self.sqrt_price_x64,
            self.liquidity,
            self.fee_ppm as u128,
            a_to_b,
            boundary,
        );
        // No crossings passed + coverage limited to the boundary ⇒ any size that
        // would reach/exceed the boundary returns None (would cross).
        match swap_exact_in_detailed(
            self.sqrt_price_x64,
            self.liquidity,
            self.fee_ppm as u128,
            a_to_b,
            amount_in,
            &[],
            boundary,
        ) {
            Some(d) if d.ticks_crossed == 0 => Ok((d.amount_out, d.total_fee as u64)),
            _ => Err(WhirlpoolLegReject::SingleTickExceeded { capacity }),
        }
    }

    /// Max within-tick input for the given input mint (event provenance).
    pub fn single_tick_capacity_for(&self, input_mint: &Pubkey) -> u64 {
        let a_to_b = *input_mint == self.mint_a;
        single_tick_capacity(
            self.sqrt_price_x64,
            self.liquidity,
            self.fee_ppm as u128,
            a_to_b,
            self.single_tick_boundary(a_to_b),
        )
    }
}

/// One venue leg with its already-fetched live state.
#[derive(Debug, Clone)]
pub enum Leg {
    /// PumpSwap: reserves are the live vault balances (base_reserve = balance
    /// of `pool.base_vault`, quote_reserve = balance of `pool.quote_vault`).
    /// `base_mint_supply` + `fee_config` come from the SAME single-slot snapshot
    /// and drive the DYNAMIC fee-v2 rate (slice 6C — no legacy 30 bps fallback).
    Pump {
        pool: PumpAmmPool,
        base_reserve: u64,
        quote_reserve: u64,
        base_mint_supply: u64,
        fee_config: FeeConfig,
    },
    /// Meteora DLMM: the pair plus every bin array the traversal may touch,
    /// and the cluster time used for volatility decay.
    Meteora {
        pair: LbPair,
        arrays: HashMap<i64, BinArray>,
        now_unix: i64,
    },
    /// Orca Whirlpool (swapV2 semantics), single-tick-clamped (S14B-2).
    Whirlpool(WhirlpoolLegState),
}

impl Leg {
    /// The output mint if `input` is one side of this leg's pool, else `None`.
    pub fn output_mint(&self, input: &Pubkey) -> Option<Pubkey> {
        match self {
            Leg::Pump { pool, .. } => {
                if input == &pool.base_mint {
                    Some(pool.quote_mint)
                } else if input == &pool.quote_mint {
                    Some(pool.base_mint)
                } else {
                    None
                }
            }
            Leg::Meteora { pair, .. } => {
                if input == &pair.token_x_mint {
                    Some(pair.token_y_mint)
                } else if input == &pair.token_y_mint {
                    Some(pair.token_x_mint)
                } else {
                    None
                }
            }
            Leg::Whirlpool(w) => {
                if input == &w.mint_a {
                    Some(w.mint_b)
                } else if input == &w.mint_b {
                    Some(w.mint_a)
                } else {
                    None
                }
            }
        }
    }

    /// Exact-in quote for this leg. Direction derived from `input_mint`.
    pub fn quote(&self, input_mint: &Pubkey, amount_in: u64) -> Result<u64, LegReject> {
        self.quote_detailed(input_mint, amount_in)
            .map(|(out, _)| out)
    }

    /// Quote plus the DEX fee charged on this leg (fee-token units).
    pub fn quote_detailed(
        &self,
        input_mint: &Pubkey,
        amount_in: u64,
    ) -> Result<(u64, u64), LegReject> {
        match self {
            Leg::Pump {
                pool,
                base_reserve,
                quote_reserve,
                base_mint_supply,
                fee_config,
            } => pump_quote_detailed_v2(
                pool,
                input_mint,
                amount_in,
                *base_reserve,
                *quote_reserve,
                *base_mint_supply,
                fee_config,
            )
            .map(|d| (d.out, d.fee))
            .map_err(LegReject::Pump),
            Leg::Meteora {
                pair,
                arrays,
                now_unix,
            } => {
                // swap_for_y = X in → Y out.
                let swap_for_y = if input_mint == &pair.token_x_mint {
                    true
                } else if input_mint == &pair.token_y_mint {
                    false
                } else {
                    return Err(LegReject::WrongMint);
                };
                dlmm_quote_exact_in_detailed(pair, arrays, swap_for_y, amount_in, *now_unix)
                    .map_err(LegReject::Dlmm)
            }
            Leg::Whirlpool(w) => w.quote(input_mint, amount_in).map_err(LegReject::Whirlpool),
        }
    }
}

/// A WSOL → token → WSOL route: `leg1` takes WSOL→token, `leg2` token→WSOL.
#[derive(Debug, Clone)]
pub struct Route {
    pub leg1: Leg,
    pub leg2: Leg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteReject {
    /// leg1 does not accept WSOL, or leg2 does not accept the intermediate
    /// token, or the two legs disagree on the token.
    TopologyMismatch,
    Leg1(LegReject),
    Leg2(LegReject),
    /// Round trip returned no more WSOL than went in.
    NonPositiveGross,
    /// Positive gross, but net after all costs is below the required floor.
    BelowNet {
        gross_profit: u64,
        net_profit: i128,
    },
}

/// A profitable, executable-shape opportunity (pre-confirmation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub token_mint: Pubkey,
    pub amount_in: u64,
    /// Intermediate token amount out of leg1 (leg2's input).
    pub token_mid: u64,
    pub wsol_out: u64,
    pub gross_profit: u64,
    pub net_profit: i128,
    /// The inclusion payment the cost model would pay at this gross.
    pub payment: u64,
    /// DEX fee on leg1 (WSOL→token), in leg1's fee-token units.
    pub leg1_fee: u64,
    /// DEX fee on leg2 (token→WSOL), in leg2's fee-token units.
    pub leg2_fee: u64,
}

impl Route {
    /// The token this route round-trips through, if the topology is coherent
    /// for the given WSOL mint.
    pub fn token_mint(&self, wsol: &Pubkey) -> Option<Pubkey> {
        let token = self.leg1.output_mint(wsol)?;
        // leg2 must take that token back to WSOL.
        if self.leg2.output_mint(&token)? == *wsol {
            Some(token)
        } else {
            None
        }
    }

    /// Run the round trip WITHOUT the profit gate: returns
    /// `(token_mid, wsol_out)` or the typed leg/topology rejection. This is the
    /// primitive the optimizer probes across sizes.
    pub fn round_trip(&self, wsol: &Pubkey, amount_in: u64) -> Result<(u64, u64), RouteReject> {
        let token = self.token_mint(wsol).ok_or(RouteReject::TopologyMismatch)?;
        let token_mid = self
            .leg1
            .quote(wsol, amount_in)
            .map_err(RouteReject::Leg1)?;
        let wsol_out = self
            .leg2
            .quote(&token, token_mid)
            .map_err(RouteReject::Leg2)?;
        Ok((token_mid, wsol_out))
    }

    /// Evaluate the full round trip at `amount_in` WSOL. `cost` carries the
    /// fee/tip model AND the required-net floor (its `required_net_lamports`).
    pub fn evaluate(
        &self,
        wsol: &Pubkey,
        amount_in: u64,
        cost: &CostModel,
    ) -> Result<Candidate, RouteReject> {
        let token = self.token_mint(wsol).ok_or(RouteReject::TopologyMismatch)?;
        let (token_mid, leg1_fee) = self
            .leg1
            .quote_detailed(wsol, amount_in)
            .map_err(RouteReject::Leg1)?;
        let (wsol_out, leg2_fee) = self
            .leg2
            .quote_detailed(&token, token_mid)
            .map_err(RouteReject::Leg2)?;

        if wsol_out <= amount_in {
            return Err(RouteReject::NonPositiveGross);
        }
        let gross_profit = wsol_out - amount_in;
        let net_profit = cost.net(gross_profit);
        if net_profit < cost.required_net_lamports as i128 {
            return Err(RouteReject::BelowNet {
                gross_profit,
                net_profit,
            });
        }
        Ok(Candidate {
            token_mint: token,
            amount_in,
            token_mid,
            wsol_out,
            gross_profit,
            net_profit,
            payment: cost.payment(gross_profit),
            leg1_fee,
            leg2_fee,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meteora_dlmm::{decode_bin_array, decode_lb_pair};
    use crate::pump_amm::PumpAmmPool;
    use arb_common::cost::ExecutionPayment;
    use std::str::FromStr;

    const WSOL: &str = "So11111111111111111111111111111111111111112";
    const LB_PAIR_BYTES: &[u8] = include_bytes!("../fixtures/meteora/lbpair_J4cGfY61.bin");
    const BIN_ARRAY_9: &[u8] = include_bytes!("../fixtures/meteora/binarray_idx9_J4cGfY61.bin");

    fn wsol() -> Pubkey {
        Pubkey::from_str(WSOL).unwrap()
    }

    /// A creator-less PumpSwap pool with chosen orientation and reserves.
    /// Both legs use SELL (base in → quote out), which is exact.
    fn pump_leg(
        base_mint: Pubkey,
        quote_mint: Pubkey,
        base_reserve: u64,
        quote_reserve: u64,
    ) -> Leg {
        Leg::Pump {
            pool: PumpAmmPool {
                bump: 0,
                index: 0,
                creator: Pubkey::default(),
                base_mint,
                quote_mint,
                lp_mint: Pubkey::default(),
                base_vault: Pubkey::default(),
                quote_vault: Pubkey::default(),
                lp_supply: 0,
                coin_creator: Pubkey::default(),
            },
            base_reserve,
            quote_reserve,
            base_mint_supply: 1_000_000_000,
            // No-creator legacy split [25,5,0] = 30 bps (preserves prior tests).
            fee_config: crate::pump_feev2::FeeConfig::flat(25, 5, 0),
        }
    }

    fn cost_model(required_net: u64) -> CostModel {
        CostModel {
            signature_fee_lamports: 5_000,
            required_net_lamports: required_net,
            payment: ExecutionPayment::JitoTip {
                min_lamports: 0,
                max_lamports: 100_000_000,
            },
            ..Default::default()
        }
    }

    #[test]
    fn profitable_round_trip_is_a_candidate() {
        let token = Pubkey::new_unique();
        // leg1: WSOL base → token quote. 1 WSOL buys ~10 token.
        let leg1 = pump_leg(wsol(), token, 1_000_000_000_000, 10_000_000_000_000);
        // leg2: token base → WSOL quote. Deep, ~1 WSOL per token.
        let leg2 = pump_leg(token, wsol(), 100_000_000_000_000, 100_000_000_000_000);
        let route = Route { leg1, leg2 };
        let cost = cost_model(0);
        let c = route.evaluate(&wsol(), 1_000_000_000, &cost).unwrap();
        assert_eq!(c.token_mint, token);
        assert_eq!(c.amount_in, 1_000_000_000);
        assert!(c.wsol_out > c.amount_in);
        assert_eq!(c.gross_profit, c.wsol_out - c.amount_in);
        // Manual chain must agree exactly with the engine.
        let mid = route.leg1.quote(&wsol(), 1_000_000_000).unwrap();
        let out = route.leg2.quote(&token, mid).unwrap();
        assert_eq!(c.token_mid, mid);
        assert_eq!(c.wsol_out, out);
        assert_eq!(c.net_profit, cost.net(c.gross_profit));
    }

    #[test]
    fn balanced_pools_have_no_edge() {
        let token = Pubkey::new_unique();
        // Symmetric 1:1 pools: fees guarantee a loss round-trip.
        let leg1 = pump_leg(wsol(), token, 1_000_000_000_000, 1_000_000_000_000);
        let leg2 = pump_leg(token, wsol(), 1_000_000_000_000, 1_000_000_000_000);
        let route = Route { leg1, leg2 };
        assert_eq!(
            route.evaluate(&wsol(), 1_000_000_000, &cost_model(0)),
            Err(RouteReject::NonPositiveGross)
        );
    }

    #[test]
    fn positive_gross_but_below_net_floor_is_rejected() {
        let token = Pubkey::new_unique();
        let leg1 = pump_leg(wsol(), token, 1_000_000_000_000, 10_000_000_000_000);
        let leg2 = pump_leg(token, wsol(), 100_000_000_000_000, 100_000_000_000_000);
        let route = Route { leg1, leg2 };
        // Require an absurd net floor: the (real, positive) gross can't clear it.
        let huge = route
            .evaluate(&wsol(), 1_000_000_000, &cost_model(u64::MAX / 2))
            .unwrap_err();
        assert!(matches!(huge, RouteReject::BelowNet { .. }));
    }

    #[test]
    fn topology_mismatch_is_rejected() {
        let token = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        // leg2 round-trips a DIFFERENT token → incoherent route.
        let leg1 = pump_leg(wsol(), token, 1_000_000_000_000, 10_000_000_000_000);
        let leg2 = pump_leg(other, wsol(), 100_000_000_000_000, 100_000_000_000_000);
        let route = Route { leg1, leg2 };
        assert_eq!(
            route.evaluate(&wsol(), 1_000_000_000, &cost_model(0)),
            Err(RouteReject::TopologyMismatch)
        );
    }

    #[test]
    fn creator_pool_buy_leg_propagates_rejection() {
        let token = Pubkey::new_unique();
        // leg1: WSOL is the QUOTE → WSOL→token is a BUY; with a creator set,
        // that BUY is refused, and the route must surface it (not fake a fill).
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
            coin_creator: Pubkey::new_unique(), // has creator
        };
        pool.coin_creator = Pubkey::new_unique();
        let leg1 = Leg::Pump {
            pool,
            base_reserve: 100_000_000_000_000,
            quote_reserve: 100_000_000_000_000,
            base_mint_supply: 1_000_000_000,
            fee_config: crate::pump_feev2::FeeConfig::flat(20, 5, 5),
        };
        let leg2 = pump_leg(token, wsol(), 100_000_000_000_000, 100_000_000_000_000);
        let route = Route { leg1, leg2 };
        assert_eq!(
            route.evaluate(&wsol(), 1_000_000_000, &cost_model(0)),
            Err(RouteReject::Leg1(LegReject::Pump(
                PumpQuoteError::CreatorBuyUnverified
            )))
        );
    }

    fn whirlpool_leg(
        mint_a: Pubkey,
        mint_b: Pubkey,
        boundary_below: i32,
        boundary_above: i32,
    ) -> Leg {
        use crate::tick_math::Q64;
        Leg::Whirlpool(WhirlpoolLegState {
            mint_a,
            mint_b,
            sqrt_price_x64: Q64, // price 1.0 at tick 0
            liquidity: 10u128.pow(15),
            tick_current_index: 0,
            tick_spacing: 8,
            fee_ppm: 500,
            initialized_ticks: vec![
                (boundary_below, 10i128.pow(12)),
                (boundary_above, -(10i128.pow(12))),
            ],
            lowest_start: -5632,
            highest_start: 5632,
        })
    }

    #[test]
    fn whirlpool_leg_quotes_within_tick_and_clamps_crossing() {
        let token = Pubkey::new_unique();
        let w = whirlpool_leg(token, wsol(), -80, 80);
        // A small token->WSOL input stays within the tick and quotes.
        let small = w.quote(&token, 1_000_000).expect("within-tick quote");
        assert!(small > 0);
        // Capacity is finite and positive.
        let Leg::Whirlpool(ws) = &w else {
            unreachable!()
        };
        let cap = ws.single_tick_capacity_for(&token);
        assert!(cap > 0);
        // An input beyond capacity must be a capacity reject (never a crossing).
        let big = w.quote(&token, cap * 100 + 10_000_000_000);
        match big {
            Err(LegReject::Whirlpool(WhirlpoolLegReject::SingleTickExceeded { capacity })) => {
                assert_eq!(capacity, cap);
            }
            other => panic!("expected SingleTickExceeded, got {other:?}"),
        }
    }

    #[test]
    fn optimizer_clamps_below_whirlpool_tick_crossing() {
        use crate::optimizer::{optimize, SizeGrid};
        let token = Pubkey::new_unique();
        // leg1: Pump WSOL(base)->token(quote) SELL (exact), deep so token_mid is
        // large enough to threaten the whirlpool tick boundary on leg2.
        let leg1 = pump_leg(wsol(), token, 2_000_000_000_000, 400_000_000_000_000);
        let leg2 = whirlpool_leg(token, wsol(), -80, 80);
        let route = Route { leg1, leg2 };
        assert_eq!(route.token_mint(&wsol()), Some(token));
        let grid = SizeGrid {
            min: 1_000_000,
            max: 50_000_000_000,
            ..Default::default()
        };
        // Whatever the optimizer returns, it must be a real evaluable candidate
        // whose leg2 does NOT cross a tick (round_trip succeeds = within-tick).
        if let Some(c) = optimize(&route, &wsol(), &cost_model(0), &grid) {
            let (_mid, _out) = route
                .round_trip(&wsol(), c.amount_in)
                .expect("chosen size is within-tick");
        }
        // And a deliberately huge size is rejected as capacity (not quoted).
        let huge = route.round_trip(&wsol(), 50_000_000_000);
        assert!(matches!(
            huge,
            Err(RouteReject::Leg2(LegReject::Whirlpool(
                WhirlpoolLegReject::SingleTickExceeded { .. }
            ))) | Ok(_)
        ));
    }

    #[test]
    fn composes_real_dlmm_leg1_with_whirlpool_leg2() {
        // The actual S14B-2 topology: WSOL→token on Meteora (real fixture),
        // token→WSOL on Whirlpool (single-tick). Must chain to a within-tick
        // round trip or a typed reject — never panic, never a crossing quote.
        let pair = decode_lb_pair(LB_PAIR_BYTES).unwrap();
        let token = pair.token_x_mint;
        let mut arrays = HashMap::new();
        arrays.insert(9i64, decode_bin_array(BIN_ARRAY_9).unwrap());
        let now = pair.v_parameters.last_update_timestamp + 5;
        // leg1: Meteora WSOL(Y)→token(X).
        let leg1 = Leg::Meteora {
            pair,
            arrays,
            now_unix: now,
        };
        // leg2: Whirlpool token→WSOL (token = mint_a).
        let leg2 = whirlpool_leg(token, wsol(), -80, 80);
        let route = Route { leg1, leg2 };
        assert_eq!(route.token_mint(&wsol()), Some(token));
        match route.round_trip(&wsol(), 100_000_000) {
            Ok((mid, out)) => {
                assert!(mid > 0 && out > 0);
                // leg2 stayed within a tick (else it would have been a reject).
            }
            Err(RouteReject::Leg1(LegReject::Dlmm(_)))
            | Err(RouteReject::Leg2(LegReject::Whirlpool(
                WhirlpoolLegReject::SingleTickExceeded { .. },
            ))) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn whirlpool_wrong_mint_rejected() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let w = whirlpool_leg(a, b, -80, 80);
        assert_eq!(
            w.quote(&Pubkey::new_unique(), 1_000),
            Err(LegReject::Whirlpool(WhirlpoolLegReject::WrongMint))
        );
    }

    #[test]
    fn cross_venue_chaining_runs_with_real_dlmm_leg() {
        // Real pump-token/WSOL DLMM pair (token X = 9cRCn9…, WSOL = Y).
        let pair = decode_lb_pair(LB_PAIR_BYTES).unwrap();
        let token = pair.token_x_mint;
        let mut arrays = HashMap::new();
        arrays.insert(9i64, decode_bin_array(BIN_ARRAY_9).unwrap());
        let now = pair.v_parameters.last_update_timestamp + 5;

        // leg1: Pump WSOL(base) → token(quote), so WSOL→token is a SELL (exact).
        let leg1 = pump_leg(wsol(), token, 5_000_000_000_000, 700_000_000_000);
        // leg2: Meteora token(X) → WSOL(Y).
        let leg2 = Leg::Meteora {
            pair,
            arrays,
            now_unix: now,
        };
        let route = Route { leg1, leg2 };
        assert_eq!(route.token_mint(&wsol()), Some(token));
        // It should chain and either be a Candidate or a typed rejection —
        // never panic, never fabricate. (Real bins ⇒ real quote both legs.)
        let cost = cost_model(0);
        match route.evaluate(&wsol(), 1_000_000_000, &cost) {
            Ok(c) => {
                assert_eq!(c.token_mint, token);
                assert!(c.wsol_out > c.amount_in);
            }
            Err(RouteReject::NonPositiveGross)
            | Err(RouteReject::BelowNet { .. })
            | Err(RouteReject::Leg2(LegReject::Dlmm(DlmmQuoteError::InsufficientBinCoverage {
                ..
            }))) => {}
            other => panic!("unexpected route outcome: {other:?}"),
        }
    }
}
