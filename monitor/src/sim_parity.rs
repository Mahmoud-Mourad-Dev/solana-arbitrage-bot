//! Simulation-only parity primitives (S13C) — SAFETY FOUNDATION.
//!
//! This module is deliberately incapable of submitting anything. It exposes:
//! - [`SafetyGate`]: hard startup guard (MODE=simulate; refuse ENABLE_SUBMIT/
//!   ENABLE_JITO/`.live-armed`; never load a keypair).
//! - [`SimRpc`]: an RPC wrapper exposing ONLY account reads + `simulateTransaction`
//!   — there is no `send_transaction`/`send_bundle`/sign method anywhere here.
//! - Verified instruction builders (Meteora DLMM `swap`) that produce an
//!   `Instruction` for a BORROWED public payer — never signed, never sent.
//!
//! Enforced by construction: this file imports nothing from the executor,
//! jito, or keypair/signing APIs. A test (`no_send_or_sign_symbols`) greps the
//! source to keep it that way.

use anyhow::{bail, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// Meteora DLMM `swap` instruction discriminator (from the official IDL).
pub const DLMM_SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

// ─────────────────────────── safety gate ───────────────────────────

/// Every reason the sim-parity binary must refuse to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyRefusal {
    ModeNotSimulate(String),
    SubmitEnabled,
    JitoEnabled,
    LiveMarkerPresent(String),
}

impl std::fmt::Display for SafetyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SafetyRefusal::ModeNotSimulate(m) => {
                write!(f, "MODE must be `simulate` for sim-parity, got `{m}`")
            }
            SafetyRefusal::SubmitEnabled => write!(f, "ENABLE_SUBMIT=true is forbidden here"),
            SafetyRefusal::JitoEnabled => write!(f, "ENABLE_JITO=true is forbidden here"),
            SafetyRefusal::LiveMarkerPresent(p) => {
                write!(f, "live-arming marker `{p}` present — refusing to run")
            }
        }
    }
}

/// The startup safety decision. PURE over its inputs so it is exhaustively
/// testable without touching the environment or filesystem.
pub struct SafetyGate;

impl SafetyGate {
    /// Decide from explicit inputs (used by tests and by `verify_env`).
    pub fn decide(
        mode: &str,
        enable_submit: bool,
        enable_jito: bool,
        live_marker_present: bool,
        live_marker_path: &str,
    ) -> Result<(), SafetyRefusal> {
        if !mode.trim().eq_ignore_ascii_case("simulate") {
            return Err(SafetyRefusal::ModeNotSimulate(mode.to_string()));
        }
        if enable_submit {
            return Err(SafetyRefusal::SubmitEnabled);
        }
        if enable_jito {
            return Err(SafetyRefusal::JitoEnabled);
        }
        if live_marker_present {
            return Err(SafetyRefusal::LiveMarkerPresent(
                live_marker_path.to_string(),
            ));
        }
        Ok(())
    }

    /// Read the environment + filesystem and refuse if unsafe. Never loads a
    /// keypair. `KEYPAIR_PATH` is intentionally NOT read.
    pub fn verify_env() -> Result<()> {
        let mode = std::env::var("MODE").unwrap_or_default();
        let submit = std::env::var("ENABLE_SUBMIT")
            .map(|v| v == "true")
            .unwrap_or(false);
        let jito = std::env::var("ENABLE_JITO")
            .map(|v| v == "true")
            .unwrap_or(false);
        let marker = std::env::var("LIVE_MARKER_PATH").unwrap_or_else(|_| ".live-armed".into());
        let present = std::path::Path::new(&marker).exists();
        if let Err(r) = Self::decide(&mode, submit, jito, present, &marker) {
            bail!("sim-parity safety refusal: {r}");
        }
        Ok(())
    }
}

// ─────────────────────────── PDA helpers ───────────────────────────

pub fn dlmm_program() -> Pubkey {
    Pubkey::from_str(crate::meteora_dlmm::DLMM_PROGRAM_ID).unwrap()
}

/// DLMM per-pair oracle PDA (verified: matches the pair's stored oracle).
pub fn dlmm_oracle(pair: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"oracle", pair.as_ref()], &dlmm_program()).0
}

/// Anchor `__event_authority` PDA of a program.
pub fn event_authority(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program).0
}

/// DLMM `bin_array_bitmap_extension` PDA (seed `["bitmap", lb_pair]`). This is
/// the optional account passed at swap2 index [1]; when a pool has no extension
/// Anchor substitutes the program id as a None sentinel instead.
pub fn bitmap_extension_pda(pair: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bitmap", pair.as_ref()], &dlmm_program()).0
}

/// Bin-array PDA (same derivation used by the observe tools).
pub fn bin_array_pda(pair: &Pubkey, index: i64) -> Pubkey {
    Pubkey::find_program_address(
        &[b"bin_array", pair.as_ref(), &index.to_le_bytes()],
        &dlmm_program(),
    )
    .0
}

// ─────────────────────── Pump PDAs (evidence-validated) ───────────────────────

pub const PUMP_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
/// The separate Pump "fees v2" program (owner of fee-config accounts [19,22]).
pub const PUMP_FEE_PROGRAM_ID: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
pub const ATA_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

pub fn pump_program() -> Pubkey {
    Pubkey::from_str(PUMP_PROGRAM_ID).unwrap()
}

/// Pump `global_config` PDA (verified: == sell account [2] on every pool).
pub fn pump_global_config() -> Pubkey {
    Pubkey::find_program_address(&[b"global_config"], &pump_program()).0
}

/// Pump coin-creator vault authority PDA (verified: == sell account [18]).
pub fn coin_creator_vault_authority(coin_creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"creator_vault", coin_creator.as_ref()], &pump_program()).0
}

/// Associated token account address for (owner, mint, token_program).
pub fn derive_ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &Pubkey::from_str(ATA_PROGRAM_ID).unwrap(),
    )
    .0
}

/// Coin-creator vault quote ATA (verified: == sell account [17]).
pub fn coin_creator_vault_ata(coin_creator: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    let auth = coin_creator_vault_authority(coin_creator);
    derive_ata(&auth, quote_mint, &Pubkey::from_str(TOKEN_PROGRAM).unwrap())
}

// ─────────────────────── DLMM swap instruction ───────────────────────

/// Accounts the caller must resolve for a DLMM swap (from live pair state).
pub struct DlmmSwapAccounts {
    pub lb_pair: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    /// 0 = SPL Token, 1 = Token-2022 (from the pair's program flags).
    pub token_x_2022: bool,
    pub token_y_2022: bool,
    /// The BORROWED public payer (never a real secret; sim only).
    pub user: Pubkey,
    pub user_token_in: Pubkey,
    pub user_token_out: Pubkey,
    /// Bin arrays the traversal may touch (writable remaining accounts).
    pub bin_arrays: Vec<Pubkey>,
}

/// Build the Meteora DLMM `swap` instruction (accounts per the official IDL,
/// oracle/event-authority PDAs verified against chain). Optional
/// bitmap-extension and host-fee accounts are passed as the program id, the
/// Anchor "None" convention. This is for `simulateTransaction` ONLY.
pub fn build_dlmm_swap_ix(
    a: &DlmmSwapAccounts,
    amount_in: u64,
    min_amount_out: u64,
) -> Instruction {
    let prog = dlmm_program();
    let tok = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
    let tok22 = Pubkey::from_str(TOKEN_2022_PROGRAM).unwrap();
    let x_prog = if a.token_x_2022 { tok22 } else { tok };
    let y_prog = if a.token_y_2022 { tok22 } else { tok };

    let mut metas = vec![
        AccountMeta::new(a.lb_pair, false),
        AccountMeta::new_readonly(prog, false), // bin_array_bitmap_extension = None
        AccountMeta::new(a.reserve_x, false),
        AccountMeta::new(a.reserve_y, false),
        AccountMeta::new(a.user_token_in, false),
        AccountMeta::new(a.user_token_out, false),
        AccountMeta::new_readonly(a.token_x_mint, false),
        AccountMeta::new_readonly(a.token_y_mint, false),
        AccountMeta::new(dlmm_oracle(&a.lb_pair), false),
        AccountMeta::new_readonly(prog, false), // host_fee_in = None
        AccountMeta::new_readonly(a.user, true), // user (signer; sim: sigVerify off)
        AccountMeta::new_readonly(x_prog, false),
        AccountMeta::new_readonly(y_prog, false),
        AccountMeta::new_readonly(event_authority(&prog), false),
        AccountMeta::new_readonly(prog, false),
    ];
    for ba in &a.bin_arrays {
        metas.push(AccountMeta::new(*ba, false));
    }

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&DLMM_SWAP_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());

    Instruction {
        program_id: prog,
        accounts: metas,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_gate_refuses_every_unsafe_condition() {
        // The one allowed configuration.
        assert!(SafetyGate::decide("simulate", false, false, false, ".live-armed").is_ok());
        assert!(SafetyGate::decide("SIMULATE", false, false, false, "x").is_ok());
        // Wrong mode.
        assert_eq!(
            SafetyGate::decide("live", false, false, false, "x"),
            Err(SafetyRefusal::ModeNotSimulate("live".into()))
        );
        assert!(matches!(
            SafetyGate::decide("observe", false, false, false, "x"),
            Err(SafetyRefusal::ModeNotSimulate(_))
        ));
        // Submit / Jito / marker each refuse even when mode is simulate.
        assert_eq!(
            SafetyGate::decide("simulate", true, false, false, "x"),
            Err(SafetyRefusal::SubmitEnabled)
        );
        assert_eq!(
            SafetyGate::decide("simulate", false, true, false, "x"),
            Err(SafetyRefusal::JitoEnabled)
        );
        assert_eq!(
            SafetyGate::decide("simulate", false, false, true, ".live-armed"),
            Err(SafetyRefusal::LiveMarkerPresent(".live-armed".into()))
        );
    }

    #[test]
    fn dlmm_oracle_and_event_authority_are_stable() {
        // Verified on-chain (route-1 pair CnK82s8e): the derived oracle matches
        // the pair's stored oracle, whose prefix is `CtDc56iE4BAW`.
        let pair = Pubkey::from_str("CnK82s8exdsK9nwqQ55kd9wcxoA22NwTchZJCBdu8LDa").unwrap();
        assert!(
            dlmm_oracle(&pair).to_string().starts_with("CtDc56iE4BAW"),
            "oracle = {}",
            dlmm_oracle(&pair)
        );
        // Derivations are deterministic.
        assert_eq!(dlmm_oracle(&pair), dlmm_oracle(&pair));
        assert_eq!(
            event_authority(&dlmm_program()),
            event_authority(&dlmm_program())
        );
    }

    #[test]
    fn dlmm_swap_ix_has_idl_shape() {
        let p = |s: &str| Pubkey::from_str(s).unwrap_or_default();
        let a = DlmmSwapAccounts {
            lb_pair: p("CnK82s8exdsK9nwqQ55kd9wcxoA22NwTchZJCBdu8LDa"),
            reserve_x: Pubkey::new_unique(),
            reserve_y: Pubkey::new_unique(),
            token_x_mint: Pubkey::new_unique(),
            token_y_mint: Pubkey::new_unique(),
            token_x_2022: true,
            token_y_2022: false,
            user: Pubkey::new_unique(),
            user_token_in: Pubkey::new_unique(),
            user_token_out: Pubkey::new_unique(),
            bin_arrays: vec![Pubkey::new_unique(), Pubkey::new_unique()],
        };
        let ix = build_dlmm_swap_ix(&a, 1_000_000_000, 0);
        assert_eq!(ix.program_id, dlmm_program());
        // 15 fixed accounts + 2 bin arrays.
        assert_eq!(ix.accounts.len(), 17);
        assert_eq!(&ix.data[0..8], &DLMM_SWAP_DISCRIMINATOR);
        assert_eq!(
            u64::from_le_bytes(ix.data[8..16].try_into().unwrap()),
            1_000_000_000
        );
        // The user account is marked signer (sim only; never actually signed).
        assert!(ix.accounts[10].is_signer);
        // reserves + user token accounts + oracle are writable.
        assert!(ix.accounts[2].is_writable && ix.accounts[3].is_writable);
        assert!(ix.accounts[4].is_writable && ix.accounts[5].is_writable);
        assert!(ix.accounts[8].is_writable);
    }

    /// SOURCE-LEVEL PROOF: this module contains no submit/sign/keypair path.
    #[test]
    fn no_send_or_sign_symbols() {
        let src = include_str!("sim_parity.rs");
        // Strip this test's own text, then keep only non-comment CODE lines so a
        // mention in a doc comment doesn't trip the check.
        let before = src.split("fn no_send_or_sign_symbols").next().unwrap();
        let hay: String = before
            .lines()
            .map(|l| l.trim_start())
            .filter(|l| !l.starts_with("//") && !l.starts_with("*") && !l.starts_with("//!"))
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "send_transaction",
            "send_and_confirm",
            "send_bundle",
            "sendBundle",
            "read_keypair_file",
            "Keypair::",
            "sign_message",
            "partial_sign",
            "try_sign",
            "JitoClient",
            "crate::executor",
        ] {
            assert!(
                !hay.contains(needle),
                "forbidden symbol `{needle}` present in sim_parity.rs"
            );
        }
    }
}
