//! Venue adapters — the single seam through which the forensics pipeline
//! touches venue-specific knowledge.
//!
//! Every program id is sourced from an existing, already-verified constant in
//! this repo (cited per adapter). None is written from memory.

use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// Raydium CLMM program.
///
/// Provenance: verified empirically in the S15A forensic scans
/// (`monitor/src/bin/forensic_arb_scan.rs`, `forensic_route_recon.rs` — pool
/// account owner of the reconstructed CLMM transactions; PoolState mints at
/// offsets 73/105 confirmed against live accounts). This is the first lib-side
/// home for the constant; the bins remain their own record.
pub const RAYDIUM_CLMM_PROGRAM_ID: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

pub trait VenueAdapter: Sync {
    /// The venue's on-chain program id.
    fn program_id(&self) -> Pubkey;
    /// Adapter name as used in input files (kebab-case).
    fn name(&self) -> &'static str;
    /// Does an instruction of `ix_program` over `accounts` touch `pool`?
    /// Default: the instruction belongs to this venue's program and the pool
    /// appears in its account list. (Meteora's 1-account event-log CPI never
    /// matches because the pool is not in its account list — which is the
    /// correct behaviour; it inflated hop counts in S15A when counted.)
    fn touches_pool(&self, ix_program: &Pubkey, accounts: &[Pubkey], pool: &Pubkey) -> bool {
        *ix_program == self.program_id() && accounts.contains(pool)
    }
    /// Extract (mint_a, mint_b) from a pool account owned by this venue's
    /// program, using the repo's existing decoders. `None` = undecodable or
    /// unsupported — never a guess.
    fn pool_mints(&self, data: &[u8]) -> Option<(Pubkey, Pubkey)>;
}

macro_rules! adapter {
    ($ty:ident, $name:literal, $prog:expr, $mints:expr) => {
        pub struct $ty;
        impl VenueAdapter for $ty {
            fn program_id(&self) -> Pubkey {
                Pubkey::from_str($prog).expect("valid base58 program id")
            }
            fn name(&self) -> &'static str {
                $name
            }
            fn pool_mints(&self, data: &[u8]) -> Option<(Pubkey, Pubkey)> {
                ($mints)(data)
            }
        }
    };
}

adapter!(
    MeteoraDlmm,
    "meteora-dlmm",
    crate::meteora_dlmm::DLMM_PROGRAM_ID,
    |d: &[u8]| {
        let p = crate::meteora_dlmm::decode_lb_pair(d).ok()?;
        Some((p.token_x_mint, p.token_y_mint))
    }
);

adapter!(
    OrcaWhirlpool,
    "orca-whirlpool",
    crate::whirlpool_parity::WHIRLPOOL_PROGRAM_ID,
    |d: &[u8]| {
        let p = crate::parsers::decode_whirlpool(d)?;
        Some((p.token_mint_a, p.token_mint_b))
    }
);

adapter!(
    RaydiumV4,
    "raydium-v4",
    arb_common::ix::RAYDIUM_V4_PROGRAM_STR,
    |d: &[u8]| {
        let p = crate::parsers::decode_raydium_v4(d)?;
        Some((p.base_mint, p.quote_mint))
    }
);

adapter!(
    PumpAmm,
    "pump-amm",
    crate::pump_amm::PUMP_AMM_PROGRAM_ID,
    |d: &[u8]| {
        let p = crate::pump_amm::decode_pump_pool(d).ok()?;
        Some((p.base_mint, p.quote_mint))
    }
);

adapter!(
    RaydiumClmm,
    "raydium-clmm",
    RAYDIUM_CLMM_PROGRAM_ID,
    |_d: &[u8]| {
        // No CLMM PoolState decoder exists in the lib crates, and the rule is
        // "reuse existing parsers, do not write new decoders". Scanning a supplied
        // CLMM pool address needs no decoding; mint DISCOVERY on this venue is
        // Unsupported until a decoder is promoted into the lib.
        None
    }
);

static METEORA: MeteoraDlmm = MeteoraDlmm;
static WHIRLPOOL: OrcaWhirlpool = OrcaWhirlpool;
static RAYDIUM_V4: RaydiumV4 = RaydiumV4;
static RAYDIUM_CLMM: RaydiumClmm = RaydiumClmm;
static PUMP: PumpAmm = PumpAmm;

/// All venues the forensics pipeline knows.
pub fn all() -> [&'static dyn VenueAdapter; 5] {
    [&METEORA, &WHIRLPOOL, &RAYDIUM_V4, &RAYDIUM_CLMM, &PUMP]
}

/// Resolve an adapter by input-file name.
pub fn adapter(name: &str) -> Option<&'static dyn VenueAdapter> {
    all().into_iter().find(|a| a.name() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_five_resolve_and_program_ids_are_valid_base58() {
        for name in [
            "meteora-dlmm",
            "orca-whirlpool",
            "raydium-v4",
            "raydium-clmm",
            "pump-amm",
        ] {
            let a = adapter(name).expect(name);
            assert_eq!(a.name(), name);
            assert_eq!(a.program_id().to_bytes().len(), 32);
        }
        assert!(adapter("serum-v3").is_none());
    }

    #[test]
    fn program_ids_match_their_cited_sources() {
        assert_eq!(
            adapter("meteora-dlmm").unwrap().program_id().to_string(),
            crate::meteora_dlmm::DLMM_PROGRAM_ID
        );
        assert_eq!(
            adapter("orca-whirlpool").unwrap().program_id().to_string(),
            crate::whirlpool_parity::WHIRLPOOL_PROGRAM_ID
        );
        assert_eq!(
            adapter("raydium-v4").unwrap().program_id().to_string(),
            arb_common::ix::RAYDIUM_V4_PROGRAM_STR
        );
        assert_eq!(
            adapter("pump-amm").unwrap().program_id().to_string(),
            crate::pump_amm::PUMP_AMM_PROGRAM_ID
        );
    }

    #[test]
    fn touches_pool_requires_program_and_pool() {
        let a = adapter("meteora-dlmm").unwrap();
        let pool = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let prog = a.program_id();
        assert!(a.touches_pool(&prog, &[other, pool], &pool));
        assert!(!a.touches_pool(&prog, &[other], &pool), "pool absent");
        assert!(
            !a.touches_pool(&Pubkey::new_unique(), &[pool], &pool),
            "wrong program"
        );
    }

    #[test]
    fn clmm_mint_discovery_is_unsupported_not_guessed() {
        assert!(adapter("raydium-clmm")
            .unwrap()
            .pool_mints(&[0u8; 1544])
            .is_none());
    }
}
