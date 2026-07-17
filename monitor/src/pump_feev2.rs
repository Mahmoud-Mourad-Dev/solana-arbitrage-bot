//! Pump AMM fee-v2 decoder + fee calculator (S13C slice 6B). PURE: no RPC.
//!
//! DISCOVERY (evidence in `docs/pump-fee-v2-layout.md`): the Pump sell fee is
//! DYNAMIC, read from the fee-program global config `[19]`
//! (`pfeeUxB6…`-owned). That account holds a fixed 24-entry tier table, each
//! entry `{ market_cap_threshold: u64, _pad: u64, lp_bps: u64=20,
//! protocol_bps: u64=5, creator_bps: u64 }` (stride 40). The applicable tier is
//! the highest whose `market_cap_threshold <= market_cap`, where
//! `market_cap = base_mint_supply * quote_reserve / base_reserve`. The total fee
//! is `lp + protocol + creator` bps, each charged with independent ceil on the
//! fee-less CPMM gross. Proven byte-exact vs on-chain simulation on Route 1
//! (creator=50 → 75 bps) and Route 3 (creator=70 → 95 bps). The legacy 30 bps
//! is simply the top tier (creator=5) for a high-market-cap pool.
//!
//! This module NEVER falls back to a hardcoded rate for a fee-v2 pool: if the
//! config cannot be decoded, it returns a typed error.

/// Anchor discriminator of the fee-program global config account ([19]).
pub const FEE_CONFIG_DISCRIMINATOR: [u8; 8] = [0x8f, 0x34, 0x92, 0xbb, 0xdb, 0x7b, 0x4c, 0x9b];
const TIER_STRIDE: usize = 40;
const MIN_TIERS: usize = 16;
/// The lp/protocol bps observed constant across every tier (structural guard).
const EXPECTED_LP_BPS: u64 = 20;
const EXPECTED_PROTOCOL_BPS: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeTier {
    /// Market cap (lamports) at/above which this tier applies.
    pub market_cap_threshold: u64,
    pub lp_bps: u64,
    pub protocol_bps: u64,
    pub creator_bps: u64,
}

impl FeeTier {
    pub fn total_bps(&self) -> u64 {
        self.lp_bps + self.protocol_bps + self.creator_bps
    }
}

/// The decoded market-cap → fee schedule (Pump fee-v2 config [19]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeConfig {
    pub tiers: Vec<FeeTier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeeV2Error {
    /// Discriminator did not match the known fee-config account.
    UnsupportedVersion,
    /// Account bytes are too short / structurally invalid.
    Malformed { reason: &'static str },
    /// No usable tier table was found.
    NoTiers,
    /// A reserve/supply was zero (cannot compute market cap).
    ZeroReserveOrSupply,
    /// Fixed-point / integer overflow.
    Overflow,
}

fn u64_at(d: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(d[off..off + 8].try_into().unwrap())
}

/// Decode the fee-v2 config: validate the discriminator, then locate the FIRST
/// contiguous run of ≥16 stride-40 tier entries whose lp/protocol bps are the
/// expected constants, market-cap thresholds strictly ascending and creator bps
/// non-increasing (the market-cap creator schedule). Structural, not a magic
/// offset — a layout change fails loudly rather than returning a wrong fee.
pub fn decode_fee_config(data: &[u8]) -> Result<FeeConfig, FeeV2Error> {
    if data.len() < 8 {
        return Err(FeeV2Error::Malformed {
            reason: "shorter than discriminator",
        });
    }
    if data[0..8] != FEE_CONFIG_DISCRIMINATOR {
        return Err(FeeV2Error::UnsupportedVersion);
    }
    // Find the first byte offset that begins a valid ascending run. Entries are
    // NOT 8-aligned (the first table starts mid-struct), so scan every byte.
    let last = data.len().saturating_sub(TIER_STRIDE);
    for off in 8..=last {
        if u64_at(data, off + 16) != EXPECTED_LP_BPS
            || u64_at(data, off + 24) != EXPECTED_PROTOCOL_BPS
        {
            continue;
        }
        let mut tiers = Vec::new();
        let mut cur = off;
        let mut prev_thr: Option<u64> = None;
        let mut prev_creator: Option<u64> = None;
        while cur + TIER_STRIDE <= data.len()
            && u64_at(data, cur + 16) == EXPECTED_LP_BPS
            && u64_at(data, cur + 24) == EXPECTED_PROTOCOL_BPS
        {
            let thr = u64_at(data, cur);
            let creator = u64_at(data, cur + 32);
            if prev_thr.is_some_and(|p| thr <= p) || prev_creator.is_some_and(|pc| creator > pc) {
                break;
            }
            tiers.push(FeeTier {
                market_cap_threshold: thr,
                lp_bps: EXPECTED_LP_BPS,
                protocol_bps: EXPECTED_PROTOCOL_BPS,
                creator_bps: creator,
            });
            prev_thr = Some(thr);
            prev_creator = Some(creator);
            cur += TIER_STRIDE;
        }
        if tiers.len() >= MIN_TIERS {
            return Ok(FeeConfig { tiers });
        }
    }
    Err(FeeV2Error::NoTiers)
}

/// Market cap (lamports) as the fee program keys the tier: the base mint's
/// circulating supply valued at the pool's implied price.
pub fn market_cap(
    base_mint_supply: u64,
    base_reserve: u64,
    quote_reserve: u64,
) -> Result<u128, FeeV2Error> {
    if base_reserve == 0 || base_mint_supply == 0 {
        return Err(FeeV2Error::ZeroReserveOrSupply);
    }
    Ok((base_mint_supply as u128)
        .checked_mul(quote_reserve as u128)
        .ok_or(FeeV2Error::Overflow)?
        / base_reserve as u128)
}

impl FeeConfig {
    /// The applicable tier: the highest whose threshold ≤ market cap. Below the
    /// first threshold, the first (highest-fee) tier applies.
    pub fn tier_for(&self, market_cap: u128) -> &FeeTier {
        let mut chosen = &self.tiers[0];
        for t in &self.tiers {
            if (t.market_cap_threshold as u128) <= market_cap {
                chosen = t;
            } else {
                break;
            }
        }
        chosen
    }
}

/// Fee breakdown on a fee-less CPMM `gross` (quote-token units), each component
/// charged with independent ceil — the exact on-chain rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeBreakdown {
    pub lp: u64,
    pub protocol: u64,
    pub creator: u64,
    pub total: u64,
}

fn ceil_bps(gross: u64, bps: u64) -> u64 {
    (((gross as u128) * (bps as u128)).div_ceil(10_000)) as u64
}

pub fn fee_breakdown(gross: u64, tier: &FeeTier) -> FeeBreakdown {
    let lp = ceil_bps(gross, tier.lp_bps);
    let protocol = ceil_bps(gross, tier.protocol_bps);
    let creator = ceil_bps(gross, tier.creator_bps);
    FeeBreakdown {
        lp,
        protocol,
        creator,
        total: lp + protocol + creator,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEE_CONFIG: &[u8] = include_bytes!("../fixtures/pump/fee_config_5PHirr8.bin");

    fn cfg() -> FeeConfig {
        decode_fee_config(FEE_CONFIG).unwrap()
    }

    #[test]
    fn decodes_24_tier_market_cap_schedule() {
        let c = cfg();
        assert_eq!(c.tiers.len(), 24);
        // First tier: 420 SOL market cap, creator 95 bps (total 120).
        assert_eq!(c.tiers[0].market_cap_threshold, 420_000_000_000);
        assert_eq!(c.tiers[0].creator_bps, 95);
        assert_eq!(c.tiers[0].total_bps(), 120);
        // Last tier: creator 5 bps → total 30 (the "legacy" rate).
        assert_eq!(c.tiers[23].creator_bps, 5);
        assert_eq!(c.tiers[23].total_bps(), 30);
        // lp/protocol constant; thresholds ascending; creator non-increasing.
        for w in c.tiers.windows(2) {
            assert!(w[1].market_cap_threshold > w[0].market_cap_threshold);
            assert!(w[1].creator_bps <= w[0].creator_bps);
            assert_eq!(w[0].lp_bps, 20);
            assert_eq!(w[0].protocol_bps, 5);
        }
    }

    #[test]
    fn wrong_discriminator_is_unsupported() {
        let mut bad = FEE_CONFIG.to_vec();
        bad[0] ^= 0xff;
        assert_eq!(decode_fee_config(&bad), Err(FeeV2Error::UnsupportedVersion));
        assert_eq!(
            decode_fee_config(&[0u8; 4]),
            Err(FeeV2Error::Malformed {
                reason: "shorter than discriminator"
            })
        );
    }

    #[test]
    fn route1_market_cap_selects_75_bps() {
        // Route 1 measured market cap ≈ 32.76e12 → tier9 (creator 50) → 75 bps.
        let c = cfg();
        let mc = market_cap(999_678_618_479_009, 52_559_268_744_521, 1_722_520_916_860).unwrap();
        let t = c.tier_for(mc);
        assert_eq!(t.creator_bps, 50);
        assert_eq!(t.total_bps(), 75);
    }

    #[test]
    fn route3_market_cap_selects_95_bps() {
        // Route 3 measured market cap ≈ 13.74e12 → tier5 (creator 70) → 95 bps.
        let c = cfg();
        let mc = market_cap(998_934_621_420_585, 58_271_548_974_899, 801_671_310_462).unwrap();
        let t = c.tier_for(mc);
        assert_eq!(t.creator_bps, 70);
        assert_eq!(t.total_bps(), 95);
    }

    #[test]
    fn fee_breakdown_reproduces_exact_measured_lamports() {
        let c = cfg();
        // Route 1 clean-bracket sample: gross 2_827_620 → real fee 21_209.
        let t1 = FeeTier {
            market_cap_threshold: 0,
            lp_bps: 20,
            protocol_bps: 5,
            creator_bps: 50,
        };
        let f1 = fee_breakdown(2_827_620, &t1);
        assert_eq!((f1.lp, f1.protocol, f1.creator), (5656, 1414, 14139));
        assert_eq!(f1.total, 21_209);
        // Route 3 clean-bracket sample: gross 26_723_357 → real fee 253_873.
        let t3 = FeeTier {
            creator_bps: 70,
            ..t1
        };
        let f3 = fee_breakdown(26_723_357, &t3);
        assert_eq!(f3.total, 253_873);
        let _ = c;
    }

    #[test]
    fn zero_reserve_is_typed_error() {
        assert_eq!(market_cap(1, 0, 1), Err(FeeV2Error::ZeroReserveOrSupply));
        assert_eq!(market_cap(0, 1, 1), Err(FeeV2Error::ZeroReserveOrSupply));
    }
}
