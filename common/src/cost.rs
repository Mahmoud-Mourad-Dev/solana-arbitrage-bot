//! Shared execution-cost model — the single source of truth both the monitor
//! (route evaluation) and the executor (pre-submit gate) MUST use, so they can
//! never disagree about whether a trade is profitable.
//!
//! Everything here is integer-only (no floating point) and saturating, matching
//! the financial-correctness bar of the rest of the bot. All amounts are in the
//! base mint's smallest unit (lamports when the base is WSOL).
//!
//! ## Why this exists
//! The pre-pivot code had a **split** cost model: the monitor assumed a fixed
//! Jito tip while the executor computed a dynamic, gross-scaled tip. A candidate
//! the monitor called profitable could be unprofitable at the executor's real
//! tip (and vice-versa). This module makes the tip (and every other cost)
//! identical on both sides.

/// The out-of-band payment made to get a transaction included, abstracted so
/// the same economics work across inclusion strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPayment {
    /// No out-of-band payment (rely on the priority fee already in the tx).
    None,
    /// A Jito tip that scales with gross profit, clamped to `[min, max]`.
    /// Identical math on both sides — see [`jito_tip`].
    JitoTip {
        min_lamports: u64,
        max_lamports: u64,
    },
    /// A fixed payment to a private relay / block builder.
    PrivateRelay { lamports: u64 },
}

impl ExecutionPayment {
    /// The payment this strategy makes for a trade of the given gross profit.
    pub fn amount(&self, gross_profit: u64) -> u64 {
        match *self {
            ExecutionPayment::None => 0,
            ExecutionPayment::JitoTip {
                min_lamports,
                max_lamports,
            } => jito_tip(gross_profit, min_lamports, max_lamports),
            ExecutionPayment::PrivateRelay { lamports } => lamports,
        }
    }
}

/// Tier for the gross-scaled Jito tip, in basis points of gross profit. Larger
/// wins pay a larger share to maximise inclusion probability. This is the exact
/// schedule the executor uses; it lives here so both sides share it byte-for-byte.
pub const fn tip_tier_bps(gross_profit_lamports: u64) -> u64 {
    const SOL: u64 = 1_000_000_000;
    match gross_profit_lamports {
        p if p < SOL / 200 => 5_000, // < 0.005 SOL: 50%
        p if p < SOL / 20 => 6_000,  // < 0.05  SOL: 60%
        p if p < SOL / 2 => 7_000,   // < 0.5   SOL: 70%
        _ => 8_000,                  // whales: 80%
    }
}

/// `clamp(min, gross * tier_bps / 10_000, max)` — integer, overflow-safe.
pub fn jito_tip(gross_profit_lamports: u64, min_tip: u64, max_tip: u64) -> u64 {
    let scaled = (gross_profit_lamports as u128 * tip_tier_bps(gross_profit_lamports) as u128
        / 10_000) as u64;
    scaled.max(min_tip).min(max_tip)
}

/// Every cost component of a single atomic arbitrage submission. Fields default
/// to zero via [`CostModel::default`]; set only what applies to your path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostModel {
    /// Base transaction signature fee (5_000 lamports per signature).
    pub signature_fee_lamports: u64,
    /// Compute-unit limit requested by the ComputeBudget instruction.
    pub compute_unit_limit: u32,
    /// Compute-unit price in micro-lamports (ComputeBudget priority fee).
    pub compute_unit_price_micro: u64,
    /// Any additional flat priority payment beyond the CU-price term.
    pub extra_priority_lamports: u64,
    /// Rent for ATAs created *in this transaction* (0 when pre-created).
    pub ata_lamports: u64,
    /// Any other one-off rent this trade must fund.
    pub rent_lamports: u64,
    /// Profit reserved as a safety buffer — not a burned cost, but kept out of
    /// the payment so a small mis-estimate does not turn the trade into a loss.
    pub margin_lamports: u64,
    /// The minimum net profit (after every cost and the margin) we insist on
    /// keeping; a trade netting less than this is rejected.
    pub required_net_lamports: u64,
    /// How we pay for inclusion.
    pub payment: ExecutionPayment,
}

impl Default for CostModel {
    fn default() -> Self {
        CostModel {
            signature_fee_lamports: 5_000,
            compute_unit_limit: 0,
            compute_unit_price_micro: 0,
            extra_priority_lamports: 0,
            ata_lamports: 0,
            rent_lamports: 0,
            margin_lamports: 0,
            required_net_lamports: 0,
            payment: ExecutionPayment::None,
        }
    }
}

impl CostModel {
    /// Lamports the ComputeBudget priority instruction costs:
    /// `cu_limit * cu_price_micro / 1_000_000`, rounded down.
    pub fn compute_priority_lamports(&self) -> u64 {
        (self.compute_unit_limit as u128 * self.compute_unit_price_micro as u128 / 1_000_000) as u64
    }

    /// All costs that DON'T depend on gross profit: signatures, compute/priority,
    /// ATA rent, and other rent. The inclusion payment and margin are excluded.
    pub fn fixed_costs(&self) -> u64 {
        self.signature_fee_lamports
            .saturating_add(self.compute_priority_lamports())
            .saturating_add(self.extra_priority_lamports)
            .saturating_add(self.ata_lamports)
            .saturating_add(self.rent_lamports)
    }

    /// The inclusion payment for this gross profit (dynamic for a Jito tip).
    pub fn payment(&self, gross_profit: u64) -> u64 {
        self.payment.amount(gross_profit)
    }

    /// Total lamports burned if the trade lands: fixed costs + inclusion payment.
    /// (Margin is a reservation, not a burn, so it is excluded here.)
    pub fn total_burn(&self, gross_profit: u64) -> u64 {
        self.fixed_costs()
            .saturating_add(self.payment(gross_profit))
    }

    /// Projected net profit after every burned cost AND the reserved margin.
    /// Signed: negative means the trade loses money once costs are paid.
    pub fn net(&self, gross_profit: u64) -> i128 {
        gross_profit as i128 - self.total_burn(gross_profit) as i128 - self.margin_lamports as i128
    }

    /// The largest inclusion payment we could afford while still keeping the
    /// margin and the required net: `gross − fixed − margin − required_net`.
    /// Zero means there is no room to pay for inclusion at all.
    pub fn max_payment(&self, gross_profit: u64) -> u64 {
        gross_profit
            .saturating_sub(self.fixed_costs())
            .saturating_sub(self.margin_lamports)
            .saturating_sub(self.required_net_lamports)
    }

    /// Whether the trade clears the bar: net (after margin) ≥ required net.
    /// Equivalent to `payment(gross) ≤ max_payment(gross)`.
    pub fn is_viable(&self, gross_profit: u64) -> bool {
        self.net(gross_profit) >= self.required_net_lamports as i128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u64 = 1_000_000_000;

    #[test]
    fn tip_tiers_match_expected_schedule() {
        assert_eq!(jito_tip(SOL / 1000, 0, u64::MAX), SOL / 1000 / 2); // 50%
        assert_eq!(jito_tip(SOL / 100, 0, u64::MAX), SOL / 100 * 6 / 10); // 60%
        assert_eq!(jito_tip(SOL / 10, 0, u64::MAX), SOL / 10 * 7 / 10); // 70%
        assert_eq!(jito_tip(SOL, 0, u64::MAX), SOL * 8 / 10); // 80%
    }

    #[test]
    fn tip_respects_clamp_and_never_overflows() {
        assert_eq!(jito_tip(10 * SOL, 0, 100_000_000), 100_000_000);
        assert_eq!(jito_tip(0, 10_000, u64::MAX), 10_000); // min floor
        assert_eq!(
            jito_tip(u64::MAX, 0, u64::MAX),
            (u64::MAX as u128 * 8 / 10) as u64
        );
    }

    #[test]
    fn fixed_costs_sum_components() {
        let m = CostModel {
            signature_fee_lamports: 5_000,
            compute_unit_limit: 700_000,
            compute_unit_price_micro: 10_000,
            ..Default::default()
        };
        // 700_000 * 10_000 / 1_000_000 = 7_000
        assert_eq!(m.compute_priority_lamports(), 7_000);
        assert_eq!(m.fixed_costs(), 12_000);
    }

    /// The property the split defect violated: monitor and executor computing
    /// net from the SAME model get the SAME answer. Here one model, two callers.
    #[test]
    fn net_equals_gross_minus_total_burn_minus_margin() {
        let m = CostModel {
            signature_fee_lamports: 5_000,
            compute_unit_limit: 700_000,
            compute_unit_price_micro: 10_000,
            margin_lamports: 10_000,
            required_net_lamports: 100_000,
            payment: ExecutionPayment::JitoTip {
                min_lamports: 10_000,
                max_lamports: 100_000_000,
            },
            ..Default::default()
        };
        let gross = SOL / 100; // 0.01 SOL
        let expected = gross as i128 - m.total_burn(gross) as i128 - 10_000;
        assert_eq!(m.net(gross), expected);
    }

    /// Two invariants that must hold for every model and gross:
    ///  1. `is_viable` is exactly the net-based check (definitional).
    ///  2. viability *implies* there is room to pay (`payment ≤ max_payment`);
    ///     and where `max_payment` hasn't saturated to 0, the two agree exactly.
    #[test]
    fn viability_matches_max_payment_invariant() {
        let models = [
            ExecutionPayment::None,
            ExecutionPayment::JitoTip {
                min_lamports: 10_000,
                max_lamports: 100_000_000,
            },
            ExecutionPayment::PrivateRelay { lamports: 50_000 },
        ];
        for payment in models {
            let m = CostModel {
                signature_fee_lamports: 5_000,
                compute_unit_limit: 700_000,
                compute_unit_price_micro: 10_000,
                margin_lamports: 10_000,
                required_net_lamports: 100_000,
                payment,
                ..Default::default()
            };
            for gross in [0u64, 50_000, 500_000, SOL / 100, SOL, 10 * SOL] {
                let viable_by_net = m.net(gross) >= m.required_net_lamports as i128;
                assert_eq!(
                    m.is_viable(gross),
                    viable_by_net,
                    "is_viable must equal net-based check (gross={gross})"
                );
                // Viability implies there is room to pay for inclusion.
                if m.is_viable(gross) {
                    assert!(
                        m.payment(gross) <= m.max_payment(gross),
                        "a viable trade must leave room for its payment (gross={gross})"
                    );
                }
                // Where max_payment hasn't saturated to 0, room ⇒ viability too.
                if m.max_payment(gross) > 0 && m.payment(gross) <= m.max_payment(gross) {
                    assert!(
                        viable_by_net,
                        "room without saturation must imply viability (gross={gross})"
                    );
                }
            }
        }
    }

    #[test]
    fn max_payment_never_eats_required_net_or_margin() {
        let m = CostModel {
            signature_fee_lamports: 5_000,
            margin_lamports: 20_000,
            required_net_lamports: 100_000,
            payment: ExecutionPayment::JitoTip {
                min_lamports: 0,
                max_lamports: u64::MAX,
            },
            ..Default::default()
        };
        let gross = SOL;
        // Paying exactly max_payment must leave net == required_net exactly.
        let room = m.max_payment(gross);
        let net_at_room =
            gross as i128 - m.fixed_costs() as i128 - room as i128 - m.margin_lamports as i128;
        assert_eq!(net_at_room, m.required_net_lamports as i128);
    }

    #[test]
    fn unprofitable_trade_is_rejected() {
        let m = CostModel {
            signature_fee_lamports: 5_000,
            required_net_lamports: 100_000,
            payment: ExecutionPayment::JitoTip {
                min_lamports: 10_000,
                max_lamports: 100_000_000,
            },
            ..Default::default()
        };
        assert!(!m.is_viable(1_000)); // gross far below costs
        assert_eq!(m.max_payment(1_000), 0);
    }
}
