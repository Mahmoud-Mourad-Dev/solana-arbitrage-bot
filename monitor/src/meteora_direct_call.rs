//! Meteora DLMM `swap2` DIRECT-CALL privilege audit (S13C slice 5, Stage 5.0/5.1).
//! PURE: no RPC, no signing, no simulation — only reasons about privileges.
//!
//! Purpose: answer, from evidence, whether the CPI-observed `swap2` instruction
//! can be invoked as a DIRECT top-level instruction (no Jupiter / no other
//! caller program), and which accounts must be substituted for our own public
//! simulation user.
//!
//! HONESTY / PROOF-TIER DISCIPLINE (per the Slice-4 correction). For a
//! CPI-exposed instruction we separate three tiers of confidence:
//!   1. Proven directly from tx metadata: program id, discriminator + data,
//!      ordered account ADDRESSES, remaining-account order, source success/slot.
//!   2. Proven by independent derivation / decoded state: oracle/event-authority
//!      /bitmap PDAs, bin-array membership, token-program ids, direction.
//!   3. NOT proven by inner metadata alone: the exact signer/writable flags the
//!      CPI caller requested, whether an account relied on caller-PDA signing,
//!      and whether the instruction runs unchanged as a top-level call.
//!
//! This module attributes every privilege flag to tier (3) → `IDL-inferred`
//! until a direct simulation (Stage 5.3) validates it. It never claims the
//! source account METAS are "privilege-exact".

use crate::meteora_reconstruct::{RouteFx, Swap2Fx};
use crate::sim_parity::dlmm_program;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub const JUPITER_V6_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
pub const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

/// Semantic role of each swap2 account, in IDL order. Index 16.. are bin arrays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swap2Role {
    LbPair,
    BinArrayBitmapExtension,
    ReserveX,
    ReserveY,
    UserTokenIn,
    UserTokenOut,
    TokenXMint,
    TokenYMint,
    Oracle,
    HostFeeIn,
    User,
    TokenXProgram,
    TokenYProgram,
    MemoProgram,
    EventAuthority,
    Program,
    BinArray,
}

impl Swap2Role {
    pub fn of(index: usize) -> Self {
        match index {
            0 => Swap2Role::LbPair,
            1 => Swap2Role::BinArrayBitmapExtension,
            2 => Swap2Role::ReserveX,
            3 => Swap2Role::ReserveY,
            4 => Swap2Role::UserTokenIn,
            5 => Swap2Role::UserTokenOut,
            6 => Swap2Role::TokenXMint,
            7 => Swap2Role::TokenYMint,
            8 => Swap2Role::Oracle,
            9 => Swap2Role::HostFeeIn,
            10 => Swap2Role::User,
            11 => Swap2Role::TokenXProgram,
            12 => Swap2Role::TokenYProgram,
            13 => Swap2Role::MemoProgram,
            14 => Swap2Role::EventAuthority,
            15 => Swap2Role::Program,
            _ => Swap2Role::BinArray,
        }
    }
}

/// Privilege the Meteora IDL assigns to a role. `signer`/`writable` here are
/// tier-3 (IDL-inferred) until direct simulation confirms them. `optional`
/// marks the two accounts that may collapse to a program-id None sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdlPrivilege {
    pub signer: bool,
    pub writable: bool,
    pub optional: bool,
}

impl Swap2Role {
    /// IDL privileges. NOTE: `User` is the ONLY signer. `writable` is the IDL's
    /// declared mutability — the captured fixtures sometimes carry a WIDER
    /// writable set (e.g. the fee-payer wallet), which is exactly the tier-3
    /// discrepancy simulation must resolve; we build with the IDL value.
    pub fn idl_privilege(self) -> IdlPrivilege {
        use Swap2Role::*;
        let (s, w, o) = match self {
            LbPair => (false, true, false),
            BinArrayBitmapExtension => (false, true, true),
            ReserveX => (false, true, false),
            ReserveY => (false, true, false),
            UserTokenIn => (false, true, false),
            UserTokenOut => (false, true, false),
            TokenXMint => (false, false, false),
            TokenYMint => (false, false, false),
            Oracle => (false, true, false),
            HostFeeIn => (false, true, true),
            User => (true, false, false),
            TokenXProgram => (false, false, false),
            TokenYProgram => (false, false, false),
            MemoProgram => (false, false, false),
            EventAuthority => (false, false, false),
            Program => (false, false, false),
            BinArray => (false, true, false),
        };
        IdlPrivilege {
            signer: s,
            writable: w,
            optional: o,
        }
    }
}

/// Can this role be safely replaced with OUR public simulation user for a
/// direct top-level call? Only the user authority and the user's own token
/// accounts are user-owned; everything else is pool/global/PDA state.
pub fn user_substitutable(role: Swap2Role) -> bool {
    matches!(
        role,
        Swap2Role::UserTokenIn | Swap2Role::UserTokenOut | Swap2Role::User
    )
}

/// One row of the Stage-5.0 privilege table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountAudit {
    pub index: usize,
    pub role: Swap2Role,
    pub address: String,
    /// Tier-3 IDL-inferred privileges (what a direct build would request).
    pub idl_signer: bool,
    pub idl_writable: bool,
    pub optional: bool,
    /// Tier-1 privileges OBSERVED in the source outer transaction message.
    pub source_signer: bool,
    pub source_writable: bool,
    /// Whether this account was the program-id None sentinel in the source.
    pub is_none_sentinel: bool,
    /// Whether, in the source, the swap authority for this fixture was a caller
    /// (Jupiter) PDA that signed via CPI rather than a top-level signer.
    pub caller_pda_signing: bool,
    /// Whether it is safe to swap this account for our public sim user.
    pub safe_to_replace: bool,
}

/// The verdict of the direct-call privilege audit for one route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectCallVerdict {
    /// Every required signer is the user authority (user-replaceable); no
    /// non-replaceable caller-PDA signer is required. Direct call is viable
    /// pending simulation.
    PrivilegesResolvedViable,
    /// A required signer is a caller PDA / not available / not replaceable.
    Unresolved { reason: String },
}

fn addr(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap_or_default()
}

/// Audit every account of a fixture against the IDL for direct-call viability.
pub fn audit_fixture(fx: &Swap2Fx) -> Vec<AccountAudit> {
    let dlmm = dlmm_program();
    // The single swap authority is account [10]. In this fixture it either
    // signed at top level (a real wallet) or was a caller PDA (Jupiter route).
    let authority_top_level_signer = fx.accounts.get(10).map(|a| a.signer).unwrap_or(false);
    let caller_pda = !authority_top_level_signer;

    fx.accounts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let role = Swap2Role::of(i);
            let priv_ = role.idl_privilege();
            let pk = addr(&rec.pubkey);
            let is_none_sentinel = pk == dlmm
                && matches!(
                    role,
                    Swap2Role::BinArrayBitmapExtension | Swap2Role::HostFeeIn
                );
            AccountAudit {
                index: i,
                role,
                address: rec.pubkey.clone(),
                idl_signer: priv_.signer,
                idl_writable: priv_.writable,
                optional: priv_.optional,
                source_signer: rec.signer,
                source_writable: rec.writable,
                is_none_sentinel,
                // Only the authority role is affected by caller-PDA signing.
                caller_pda_signing: role == Swap2Role::User && caller_pda,
                safe_to_replace: user_substitutable(role),
            }
        })
        .collect()
}

/// Decide direct-call viability from a route's audited fixtures.
///
/// Viable iff the ONLY IDL signer role is `User` and that role is
/// user-substitutable — i.e. even where the source used a Jupiter PDA to sign,
/// a direct call simply supplies OUR own authority instead. No other account
/// requires a signature, so no non-replaceable caller-PDA signer exists.
pub fn verdict(route: &RouteFx) -> DirectCallVerdict {
    if route.cpi_fixtures.is_empty() {
        return DirectCallVerdict::Unresolved {
            reason: "no fixtures to audit".into(),
        };
    }
    for fx in &route.cpi_fixtures {
        let rows = audit_fixture(fx);
        let signer_rows: Vec<&AccountAudit> = rows.iter().filter(|r| r.idl_signer).collect();
        if signer_rows.len() != 1 {
            return DirectCallVerdict::Unresolved {
                reason: format!(
                    "expected exactly one IDL signer, found {} in {}",
                    signer_rows.len(),
                    fx.sig
                ),
            };
        }
        let s = signer_rows[0];
        if s.role != Swap2Role::User || !s.safe_to_replace {
            return DirectCallVerdict::Unresolved {
                reason: format!("sole signer is {:?}, not user-replaceable", s.role),
            };
        }
    }
    DirectCallVerdict::PrivilegesResolvedViable
}

// ─────────────────── Stage 5.1: builder-vs-source meta diff ───────────────────

/// A difference between what a DIRECT builder would request for an account and
/// what the source outer transaction actually carried. Recorded, never hidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaDiff {
    pub index: usize,
    pub role: Swap2Role,
    pub field: &'static str,
    pub builder: bool,
    pub source: bool,
}

/// Compare IDL-inferred (builder) privileges to source-observed privileges for
/// the substitutable user accounts and the authority. Non-user accounts are
/// pool/PDA state and are expected to match on address, not privilege — the
/// meaningful privilege deltas live on [4],[5],[10]. Returns all deltas.
pub fn builder_vs_source_diff(fx: &Swap2Fx) -> Vec<MetaDiff> {
    let mut out = Vec::new();
    for row in audit_fixture(fx) {
        if row.idl_signer != row.source_signer {
            out.push(MetaDiff {
                index: row.index,
                role: row.role,
                field: "signer",
                builder: row.idl_signer,
                source: row.source_signer,
            });
        }
        if row.idl_writable != row.source_writable && !row.is_none_sentinel {
            out.push(MetaDiff {
                index: row.index,
                role: row.role,
                field: "writable",
                builder: row.idl_writable,
                source: row.source_writable,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meteora_reconstruct::{load, TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
    use crate::sim_parity::{dlmm_oracle, event_authority};

    fn route1() -> RouteFx {
        load()
            .routes
            .into_values()
            .find(|r| r.route == "route1")
            .unwrap()
    }

    #[test]
    fn exactly_one_idl_signer_and_it_is_the_user() {
        let signers: Vec<Swap2Role> = (0..16)
            .map(Swap2Role::of)
            .filter(|r| r.idl_privilege().signer)
            .collect();
        assert_eq!(
            signers,
            vec![Swap2Role::User],
            "only the user authority signs"
        );
    }

    #[test]
    fn role_mapping_matches_fixture_landmarks() {
        // Landmarks proven in slice 4 pin the role order to the real accounts.
        let r = route1();
        let pair = Pubkey::from_str(&r.pair).unwrap();
        let fx = &r.cpi_fixtures[0];
        assert_eq!(Swap2Role::of(0), Swap2Role::LbPair);
        assert_eq!(fx.accounts[0].pubkey, r.pair);
        assert_eq!(Swap2Role::of(8), Swap2Role::Oracle);
        assert_eq!(fx.accounts[8].pubkey, dlmm_oracle(&pair).to_string());
        assert_eq!(Swap2Role::of(14), Swap2Role::EventAuthority);
        assert_eq!(
            fx.accounts[14].pubkey,
            event_authority(&dlmm_program()).to_string()
        );
        assert_eq!(Swap2Role::of(13), Swap2Role::MemoProgram);
        assert_eq!(fx.accounts[13].pubkey, MEMO_PROGRAM);
        assert_eq!(Swap2Role::of(11), Swap2Role::TokenXProgram);
        assert_eq!(fx.accounts[11].pubkey, TOKEN_2022_PROGRAM);
        assert_eq!(fx.accounts[12].pubkey, TOKEN_PROGRAM);
    }

    #[test]
    fn direct_call_verdict_is_viable_for_route1() {
        assert_eq!(
            verdict(&route1()),
            DirectCallVerdict::PrivilegesResolvedViable
        );
    }

    #[test]
    fn authority_is_caller_pda_in_some_fixtures_and_top_level_signer_in_others() {
        // This is the empirical heart of the audit: fixture 2 carries a REAL
        // top-level signer authority (proving swap2 accepts an ordinary wallet),
        // while fixtures 1 & 3 used a Jupiter PDA signed by CPI.
        let r = route1();
        let mut top_level = 0;
        let mut pda = 0;
        for fx in &r.cpi_fixtures {
            let rows = audit_fixture(fx);
            let user = rows.iter().find(|x| x.role == Swap2Role::User).unwrap();
            if user.caller_pda_signing {
                pda += 1;
            } else {
                top_level += 1;
            }
        }
        assert!(
            top_level >= 1,
            "≥1 fixture proves an ordinary-wallet authority"
        );
        assert!(
            pda >= 1,
            "≥1 fixture used caller-PDA signing (Jupiter route)"
        );
    }

    #[test]
    fn only_user_accounts_are_replaceable() {
        for i in 0..16 {
            let role = Swap2Role::of(i);
            let expect = matches!(i, 4 | 5 | 10);
            assert_eq!(
                user_substitutable(role),
                expect,
                "role {role:?} at {i} replaceability"
            );
        }
    }

    #[test]
    fn builder_vs_source_diff_is_recorded_not_hidden() {
        // The fixtures carry the authority [10] as WRITABLE (fee payer / Jupiter
        // marking) whereas the IDL declares user readonly — a genuine tier-3
        // privilege delta that must surface for simulation to resolve.
        let r = route1();
        let mut saw_user_writable_delta = false;
        for fx in &r.cpi_fixtures {
            for d in builder_vs_source_diff(fx) {
                if d.role == Swap2Role::User && d.field == "writable" {
                    saw_user_writable_delta = true;
                    assert!(!d.builder && d.source, "IDL readonly vs source writable");
                }
            }
        }
        assert!(
            saw_user_writable_delta,
            "expected the known user writable discrepancy to be reported"
        );
    }
}
