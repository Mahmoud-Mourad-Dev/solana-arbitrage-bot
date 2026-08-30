//! PumpSwap AMM PDA derivations — the SINGLE off-chain definition, shared by
//! the monitor and the executor (Prompt P, mirroring [`crate::dlmm_pda`]).
//! Gated behind the off-chain-only `pda` feature so the `no_std` on-chain
//! program never pulls solana-pubkey.
//!
//! Seeds are the ones proven in `monitor/src/sim_parity.rs` (each helper there
//! is documented "verified: == sell account [N]" against captured mainnet
//! sells). This module is the shared home for them, cited per function:
//! - global_config:              `["global_config"]`            (sim_parity: sell [2])
//! - event authority:            `["__event_authority"]`        (Anchor; sell [15])
//! - coin-creator vault auth:    `["creator_vault", creator]`   (sim_parity: sell [18])
//! - coin-creator vault ATA:     ATA(auth, quote_mint, token)   (sim_parity: sell [17])
//!
//! IMPORTANT (cited from `docs/pump-fee-v2-layout.md`): the fee-v2 accounts
//! [19],[21],[22],[23] and the rotating protocol-fee recipient [9],[10] have
//! **no reproducible derivation** — their seeds are undocumented and [9],[10],
//! [22],[23] rotate per recipient. They CANNOT be produced here and must be
//! carried from a recent on-chain transaction. This module deliberately does
//! not fabricate them.

use solana_pubkey::Pubkey;

/// Pump `global_config` PDA (sell account [2]).
pub fn global_config(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"global_config"], program).0
}

/// Anchor `__event_authority` PDA (sell account [15]).
pub fn event_authority(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program).0
}

/// Coin-creator vault authority PDA (sell account [18]).
pub fn coin_creator_vault_authority(program: &Pubkey, coin_creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"creator_vault", coin_creator.as_ref()], program).0
}

/// Associated token account (owner, mint, token_program).
pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey, ata_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        ata_program,
    )
    .0
}

/// Coin-creator vault quote ATA (sell account [17]).
pub fn coin_creator_vault_ata(
    program: &Pubkey,
    coin_creator: &Pubkey,
    quote_mint: &Pubkey,
    token_program: &Pubkey,
    ata_program: &Pubkey,
) -> Pubkey {
    let auth = coin_creator_vault_authority(program, coin_creator);
    ata(&auth, quote_mint, token_program, ata_program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::str::FromStr;

    fn pump() -> Pubkey {
        Pubkey::from_str(crate::ix::PUMP_AMM_PROGRAM_STR).unwrap()
    }

    /// The two pool-independent PDAs must reproduce the values captured in the
    /// mainnet sell CPI fixture (monitor/fixtures/pump/reconstruction_fixtures
    /// route1): sell account [2] and [15]. This is the independent
    /// re-derivation check the fee-v2 layout doc calls "Proven by PDA
    /// re-derivation".
    #[test]
    fn pool_independent_pdas_match_captured_cpi() {
        assert_eq!(
            global_config(&pump()).to_string(),
            "ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw",
            "global_config [2]"
        );
        assert_eq!(
            event_authority(&pump()).to_string(),
            "GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR",
            "event_authority [15]"
        );
    }

    #[test]
    fn creator_vault_derivations_are_deterministic() {
        let creator = Pubkey::new_unique();
        let quote = Pubkey::new_unique();
        let tok = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        let atap = Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap();
        let a = coin_creator_vault_ata(&pump(), &creator, &quote, &tok, &atap);
        assert_eq!(
            a,
            coin_creator_vault_ata(&pump(), &creator, &quote, &tok, &atap)
        );
        assert_ne!(a, coin_creator_vault_authority(&pump(), &creator));
    }
}
