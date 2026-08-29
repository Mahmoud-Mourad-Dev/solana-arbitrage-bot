//! Meteora DLMM PDA derivations — the SINGLE off-chain definition, shared by
//! the monitor (quote/reconstruct) and the executor (account resolution).
//!
//! Previously these lived only in `monitor/src/sim_parity.rs`; duplicating
//! them in the executor would be exactly the split-source drift bug that
//! `common/src/cost.rs` exists to prevent. Gated behind the off-chain-only
//! `pda` feature so the `no_std` on-chain program never pulls solana-pubkey.
//!
//! Seeds are unchanged from the monitor's verified derivations:
//! - oracle:            `["oracle", lb_pair]`
//! - event authority:   `["__event_authority"]` (Anchor)
//! - bitmap extension:  `["bitmap", lb_pair]`
//! - bin array:         `["bin_array", lb_pair, index_i64_le]`

use solana_pubkey::Pubkey;

/// DLMM per-pair oracle PDA.
pub fn dlmm_oracle(program: &Pubkey, pair: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"oracle", pair.as_ref()], program).0
}

/// Anchor `__event_authority` PDA of a program.
pub fn event_authority(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program).0
}

/// DLMM `bin_array_bitmap_extension` PDA. When a pool has no extension the
/// program id is substituted as a None sentinel at the call site — this
/// returns the real PDA; the caller decides which to pass.
pub fn bitmap_extension_pda(program: &Pubkey, pair: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bitmap", pair.as_ref()], program).0
}

/// Bin-array PDA for a given signed array index.
pub fn bin_array_pda(program: &Pubkey, pair: &Pubkey, index: i64) -> Pubkey {
    Pubkey::find_program_address(
        &[b"bin_array", pair.as_ref(), &index.to_le_bytes()],
        program,
    )
    .0
}

/// Bin id → containing bin-array index (70 bins per array; floor semantics).
pub const BINS_PER_ARRAY: i32 = 70;
pub fn bin_array_index(bin_id: i32) -> i64 {
    bin_id.div_euclid(BINS_PER_ARRAY) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::str::FromStr;

    fn prog() -> Pubkey {
        Pubkey::from_str(crate::ix::METEORA_DLMM_PROGRAM_STR).unwrap()
    }

    #[test]
    fn derivations_are_deterministic_and_distinct() {
        let pair = Pubkey::new_unique();
        let p = prog();
        assert_eq!(dlmm_oracle(&p, &pair), dlmm_oracle(&p, &pair));
        assert_ne!(dlmm_oracle(&p, &pair), bitmap_extension_pda(&p, &pair));
        assert_ne!(bin_array_pda(&p, &pair, -1), bin_array_pda(&p, &pair, 0));
    }

    #[test]
    fn bin_array_index_floor_semantics() {
        assert_eq!(bin_array_index(0), 0);
        assert_eq!(bin_array_index(69), 0);
        assert_eq!(bin_array_index(70), 1);
        assert_eq!(bin_array_index(-1), -1);
        assert_eq!(bin_array_index(-70), -1);
        assert_eq!(bin_array_index(-71), -2);
    }
}
