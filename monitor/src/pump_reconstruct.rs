//! Byte-exact Pump `sell` reconstruction (S13C slice 3). PURE: no RPC, no
//! transaction construction, no payer substitution, no simulation.
//!
//! Given a validated fixture template + semantic (`base_amount_in`,
//! `min_quote_out`), reconstruct the exact Pump sell instruction and prove it
//! reproduces the original bytes and account metas. Also provides the provenance
//! guard, the cross-fixture account matrix, the rotating-recipient FRESHNESS
//! validator (a pure predicate over account data — the caller fetches), and the
//! fee-v2 clone-required association checks.
//!
//! Fee-v2 accounts [19,21,22,23] keep the status
//! `CLONE_REQUIRED — PDA DERIVATION UNRESOLVED`; nothing here derives them.

use crate::fixture_capture::{AccountMetaRec, Fixture};
use crate::sim_client::AccountData;
use serde::Deserialize;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub const SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

// ─────────────────────────── fixture loading ───────────────────────────

#[derive(Debug, Deserialize)]
pub struct ReconstructionFixtures {
    pub routes: std::collections::BTreeMap<String, RouteFixtures>,
}

#[derive(Debug, Deserialize)]
pub struct RouteFixtures {
    pub route: String,
    pub pool: String,
    pub mint: String,
    pub status: String,
    pub fixtures: Vec<Fixture>,
}

pub const FIXTURES_JSON: &str = include_str!("../fixtures/pump/reconstruction_fixtures.json");

pub fn load() -> ReconstructionFixtures {
    serde_json::from_str(FIXTURES_JSON).expect("reconstruction_fixtures.json parses")
}

// ─────────────────────── instruction-data reconstruction ───────────────────────

/// Reconstruct the 24-byte sell data from semantic fields (LE u64s).
pub fn reconstruct_sell_data(base_amount_in: u64, min_quote_out: u64) -> [u8; 24] {
    let mut d = [0u8; 24];
    d[0..8].copy_from_slice(&SELL_DISCRIMINATOR);
    d[8..16].copy_from_slice(&base_amount_in.to_le_bytes());
    d[16..24].copy_from_slice(&min_quote_out.to_le_bytes());
    d
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataError {
    BadLength { len: usize },
    WrongDiscriminator,
}

/// Decode + strictly validate raw sell data. Rejects malformed lengths and a
/// wrong discriminator — no unexplained bytes are tolerated.
pub fn decode_sell_data(bytes: &[u8]) -> Result<(u64, u64), DataError> {
    if bytes.len() != 24 {
        return Err(DataError::BadLength { len: bytes.len() });
    }
    if bytes[0..8] != SELL_DISCRIMINATOR {
        return Err(DataError::WrongDiscriminator);
    }
    Ok((
        u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
    ))
}

// ─────────────────────── account-meta reconstruction ───────────────────────

fn meta(rec: &AccountMetaRec) -> AccountMeta {
    let pk = Pubkey::from_str(&rec.pubkey).unwrap_or_default();
    if rec.writable {
        AccountMeta::new(pk, rec.signer)
    } else {
        AccountMeta::new_readonly(pk, rec.signer)
    }
}

/// Typed reasons a fixture cannot be used for reconstruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    NotDirectTopLevel,
    PoolMismatch,
    MintMismatch,
    ProgramDeploymentChanged,
    AccountCountNot24 { got: usize },
    DataMismatch,
    FeeV2OwnershipFailed { index: usize },
}

/// Provenance guard — a fixture is usable ONLY if every condition holds.
/// `deployment_matches` is the program-version comparison from capture.
pub fn provenance_guard(
    fx: &Fixture,
    expected_pool: &str,
    expected_mint: &str,
    deployment_matches: bool,
) -> Result<(), RejectReason> {
    if fx.class != crate::fixture_capture::TxClass::DirectTopLevel {
        return Err(RejectReason::NotDirectTopLevel);
    }
    if fx.accounts.len() != 24 {
        return Err(RejectReason::AccountCountNot24 {
            got: fx.accounts.len(),
        });
    }
    if fx.accounts[0].pubkey != expected_pool {
        return Err(RejectReason::PoolMismatch);
    }
    if fx.accounts[3].pubkey != expected_mint {
        return Err(RejectReason::MintMismatch);
    }
    if !deployment_matches {
        return Err(RejectReason::ProgramDeploymentChanged);
    }
    // Instruction data must reconstruct exactly.
    let raw = crate::fixture_capture::b58_decode(&fx.data_b58).unwrap_or_default();
    let (a, m) = decode_sell_data(&raw).map_err(|_| RejectReason::DataMismatch)?;
    if reconstruct_sell_data(a, m).as_slice() != raw.as_slice() {
        return Err(RejectReason::DataMismatch);
    }
    Ok(())
}

/// Reconstruct the exact Pump sell instruction from a validated fixture,
/// overriding only the semantic amounts. PURE — no RPC, no side effects, and
/// NO payer substitution (see [`substitute_user_accounts`] for that, kept as a
/// separate pure account-meta transform).
pub fn reconstruct_sell_instruction(
    fx: &Fixture,
    base_amount_in: u64,
    min_quote_out: u64,
) -> Instruction {
    let accounts = fx.accounts.iter().map(meta).collect();
    Instruction {
        program_id: Pubkey::from_str(&fx.program_id).unwrap_or_default(),
        accounts,
        data: reconstruct_sell_data(base_amount_in, min_quote_out).to_vec(),
    }
}

/// Separate, pure account-meta transform: replace ONLY the user-specific
/// accounts [1] authority, [5] base ATA, [6] quote ATA. Builds no transaction.
/// (Not used until later slices; provided + tested so the substitution set is
/// explicit and auditable.)
pub fn substitute_user_accounts(
    metas: &[AccountMetaRec],
    user_authority: &str,
    user_base_ata: &str,
    user_quote_ata: &str,
) -> Vec<AccountMetaRec> {
    metas
        .iter()
        .cloned()
        .map(|mut a| {
            match a.index {
                1 => a.pubkey = user_authority.to_string(),
                5 => a.pubkey = user_base_ata.to_string(),
                6 => a.pubkey = user_quote_ata.to_string(),
                _ => {}
            }
            a
        })
        .collect()
}

// ─────────────────────── cross-fixture matrix ───────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexClass {
    /// Identical across ALL fixtures (every pool).
    GlobalConstant,
    /// Constant within a pool, differs between pools.
    PoolConstant,
    /// Varies within a single pool (rotating fee recipient / user).
    VariesWithinPool,
}

/// Classify each of the 24 account indices across a set of routes' fixtures.
pub fn cross_fixture_matrix(routes: &[&RouteFixtures]) -> [IndexClass; 24] {
    let mut out = [IndexClass::GlobalConstant; 24];
    for (idx, slot) in out.iter_mut().enumerate() {
        let mut global: std::collections::BTreeSet<String> = Default::default();
        let mut varies_within = false;
        for r in routes {
            let mut per_pool: std::collections::BTreeSet<String> = Default::default();
            for fx in &r.fixtures {
                if let Some(a) = fx.accounts.get(idx) {
                    per_pool.insert(a.pubkey.clone());
                    global.insert(a.pubkey.clone());
                }
            }
            if per_pool.len() > 1 {
                varies_within = true;
            }
        }
        *slot = if varies_within {
            IndexClass::VariesWithinPool
        } else if global.len() <= 1 {
            IndexClass::GlobalConstant
        } else {
            IndexClass::PoolConstant
        };
    }
    out
}

// ─────────────────────── rotating recipient freshness ───────────────────────

pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshnessReject {
    RecipientMissing,
    RecipientAtaMissing,
    WrongAtaOwnerProgram,
    WrongMint,
    InvalidAccountData,
    DeploymentChanged,
    OutsidePolicy,
}

/// PURE freshness validator for a cloned protocol-fee recipient pair
/// (accounts [9]=recipient, [10]=recipient ATA). The caller fetches the current
/// account data; this predicate rejects a stale/invalid pair. It does NOT
/// simulate. `within_policy` is the caller's freshness-window decision (e.g. the
/// pair was seen in a successful tx within N slots).
pub fn validate_recipient_freshness(
    recipient: Option<&AccountData>,
    recipient_ata: Option<&AccountData>,
    expected_quote_mint: &str,
    deployment_changed: bool,
    within_policy: bool,
) -> Result<(), FreshnessReject> {
    if deployment_changed {
        return Err(FreshnessReject::DeploymentChanged);
    }
    if !within_policy {
        return Err(FreshnessReject::OutsidePolicy);
    }
    let _rec = recipient.ok_or(FreshnessReject::RecipientMissing)?;
    let ata = recipient_ata.ok_or(FreshnessReject::RecipientAtaMissing)?;
    // ATA must be a token account owned by the SPL Token program.
    if ata.owner != Pubkey::from_str(TOKEN_PROGRAM).unwrap() {
        return Err(FreshnessReject::WrongAtaOwnerProgram);
    }
    if ata.data.len() < 64 {
        return Err(FreshnessReject::InvalidAccountData);
    }
    // SPL token account: mint at bytes 0..32.
    let mint = Pubkey::new_from_array(ata.data[0..32].try_into().unwrap());
    if mint != Pubkey::from_str(expected_quote_mint).unwrap_or_default() {
        return Err(FreshnessReject::WrongMint);
    }
    Ok(())
}

// ─────────────────────── fee-v2 association guard ───────────────────────

/// The fee-v2 clone-required indices. PDA seeds are UNRESOLVED — never derived.
/// Verified roles (3 distinct-seller fixtures per pool):
/// - [19] global fee CONFIG (same across pools), [20] the fee PROGRAM (global).
/// - [21] pool-specific fee account (constant within a pool).
/// - [22],[23] ROTATE WITH the protocol-fee recipient [9],[10] — they are NOT
///   pool-constant. They must be cloned as a COHERENT SET from ONE source
///   transaction and refreshed together (see [`RECIPIENT_ROTATING_INDICES`]).
pub const FEE_V2_INDICES: [usize; 4] = [19, 21, 22, 23];
pub const FEE_V2_STATUS: &str = "CLONE_REQUIRED — PDA DERIVATION UNRESOLVED";

/// The accounts that rotate together per protocol-fee recipient. Cloning a
/// recipient means cloning ALL of these from the SAME source sell.
pub const RECIPIENT_ROTATING_INDICES: [usize; 4] = [9, 10, 22, 23];

/// Indices that are POOL-SPECIFIC (constant within a pool, differ between
/// pools). Reusing any of these from a different pool is contamination.
pub const POOL_SPECIFIC_INDICES: [usize; 7] = [0, 3, 7, 8, 17, 18, 21];

/// Reject reusing pool-specific accounts from a DIFFERENT pool. Returns the
/// first offending index. (Rotating indices [9,10,22,23] are excluded — they
/// vary per tx even within a pool and are validated by freshness instead.)
pub fn reject_cross_pool_fee_v2(
    target: &Fixture,
    other_pool: &Fixture,
) -> Result<(), RejectReason> {
    for idx in POOL_SPECIFIC_INDICES {
        if let (Some(a), Some(b)) = (target.accounts.get(idx), other_pool.accounts.get(idx)) {
            if a.pubkey == b.pubkey {
                return Err(RejectReason::FeeV2OwnershipFailed { index: idx });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route1() -> RouteFixtures {
        let all = load();
        all.routes
            .into_values()
            .find(|r| r.route == "route1")
            .unwrap()
    }
    fn route3() -> RouteFixtures {
        let all = load();
        all.routes
            .into_values()
            .find(|r| r.route == "route3")
            .unwrap()
    }

    #[test]
    fn fixtures_have_three_direct_sells_from_distinct_sellers() {
        for r in [route1(), route3()] {
            assert_eq!(r.status, "SUFFICIENT");
            assert!(r.fixtures.len() >= 3, "{} needs ≥3 fixtures", r.route);
            let sellers: std::collections::BTreeSet<_> = r
                .fixtures
                .iter()
                .map(|f| f.accounts[1].pubkey.clone())
                .collect();
            assert!(sellers.len() >= 3, "{} needs distinct sellers", r.route);
        }
    }

    #[test]
    fn instruction_data_reconstructs_byte_exact_for_every_fixture() {
        for r in [route1(), route3()] {
            for fx in &r.fixtures {
                let raw = crate::fixture_capture::b58_decode(&fx.data_b58).unwrap();
                let (a, m) = decode_sell_data(&raw).unwrap();
                let d = fx.decoded.as_ref().unwrap();
                assert_eq!((a, m), (d.amount_in, d.min_out));
                assert_eq!(reconstruct_sell_data(a, m).as_slice(), raw.as_slice());
            }
        }
    }

    #[test]
    fn field_mutations_touch_only_their_bytes() {
        let base = reconstruct_sell_data(1000, 2000);
        let ma = reconstruct_sell_data(1001, 2000);
        let mm = reconstruct_sell_data(1000, 2001);
        assert_eq!(&base[0..8], &SELL_DISCRIMINATOR); // disc unchanged
                                                      // amount change: only bytes 8..16 differ.
        assert_eq!(base[0..8], ma[0..8]);
        assert_ne!(base[8..16], ma[8..16]);
        assert_eq!(base[16..24], ma[16..24]);
        // min_out change: only bytes 16..24 differ.
        assert_eq!(base[0..16], mm[0..16]);
        assert_ne!(base[16..24], mm[16..24]);
        // Endianness is little-endian.
        assert_eq!(
            &reconstruct_sell_data(0x0102030405060708, 0)[8..16],
            &0x0102030405060708u64.to_le_bytes()
        );
    }

    #[test]
    fn decode_rejects_malformed_and_wrong_discriminator() {
        assert_eq!(
            decode_sell_data(&[0u8; 23]),
            Err(DataError::BadLength { len: 23 })
        );
        assert_eq!(
            decode_sell_data(&[0u8; 25]),
            Err(DataError::BadLength { len: 25 })
        );
        let mut bad = reconstruct_sell_data(1, 2).to_vec();
        bad[0] ^= 0xff;
        assert_eq!(decode_sell_data(&bad), Err(DataError::WrongDiscriminator));
    }

    #[test]
    fn instruction_reconstruction_preserves_metas_and_flags() {
        let r = route1();
        let fx = &r.fixtures[0];
        let ix = reconstruct_sell_instruction(fx, 123, 45);
        assert_eq!(ix.accounts.len(), 24);
        for (rec, am) in fx.accounts.iter().zip(&ix.accounts) {
            assert_eq!(am.pubkey.to_string(), rec.pubkey);
            assert_eq!(am.is_signer, rec.signer);
            assert_eq!(am.is_writable, rec.writable);
        }
        assert_eq!(&ix.data[0..8], &SELL_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 123);
    }

    #[test]
    fn provenance_guard_accepts_valid_rejects_each_failure() {
        let r = route1();
        let fx = &r.fixtures[0];
        assert!(provenance_guard(fx, &r.pool, &r.mint, true).is_ok());
        assert_eq!(
            provenance_guard(fx, "WRONGPOOL", &r.mint, true),
            Err(RejectReason::PoolMismatch)
        );
        assert_eq!(
            provenance_guard(fx, &r.pool, "WRONGMINT", true),
            Err(RejectReason::MintMismatch)
        );
        assert_eq!(
            provenance_guard(fx, &r.pool, &r.mint, false),
            Err(RejectReason::ProgramDeploymentChanged)
        );
    }

    #[test]
    fn user_specific_indices_vary_by_seller_others_do_not() {
        // Across the 3 distinct-seller fixtures, indices 1/5/6 must all differ;
        // pool/global indices must be constant. Report any OTHER varying index.
        for r in [route1(), route3()] {
            let n = r.fixtures.len();
            let distinct = |idx: usize| {
                r.fixtures
                    .iter()
                    .map(|f| f.accounts[idx].pubkey.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
            };
            for idx in [1usize, 5, 6] {
                assert!(
                    distinct(idx) >= 2,
                    "{} idx {idx} should vary by seller",
                    r.route
                );
            }
            // Pool-specific + global indices constant across sellers of a pool.
            // NOTE: [22],[23] rotate with the recipient, so they are NOT here.
            for idx in [0usize, 3, 7, 8, 17, 18, 19, 20, 21] {
                assert_eq!(
                    distinct(idx),
                    1,
                    "{} idx {idx} must be constant per pool",
                    r.route
                );
            }
            // Surface any UNEXPECTED varying index (besides the known rotating /
            // user set 1,5,6,9,10,22,23).
            let unexpected: Vec<usize> = (0..24)
                .filter(|&i| ![1, 5, 6, 9, 10, 22, 23].contains(&i) && distinct(i) > 1)
                .collect();
            assert!(
                unexpected.is_empty(),
                "{} unexpected varying indices {unexpected:?}",
                r.route
            );
            let _ = n;
        }
    }

    #[test]
    fn cross_pool_reuse_of_pool_specific_accounts_is_rejected() {
        let r1 = route1();
        let r3 = route3();
        // Different pools ⇒ no pool-specific/fee-v2 account should coincide.
        assert!(reject_cross_pool_fee_v2(&r1.fixtures[0], &r3.fixtures[0]).is_ok());
        // Reusing route1's own fixture as "other pool" ⇒ everything coincides ⇒
        // caught (pool index 0 identical).
        assert_eq!(
            reject_cross_pool_fee_v2(&r1.fixtures[0], &r1.fixtures[1]),
            Err(RejectReason::FeeV2OwnershipFailed { index: 0 })
        );
    }

    #[test]
    fn matrix_classifies_global_pool_and_varying_indices() {
        let r1 = route1();
        let r3 = route3();
        let m = cross_fixture_matrix(&[&r1, &r3]);
        // Globals: program ids, system, ATA prog, event authority, global config,
        // fee program + fee config.
        for idx in [2usize, 13, 14, 15, 16, 19, 20] {
            assert_eq!(m[idx], IndexClass::GlobalConstant, "idx {idx}");
        }
        // Pool-specific: pool, vaults, cc vault accounts, [21] pool fee account.
        for idx in [0usize, 7, 8, 17, 18, 21] {
            assert_eq!(m[idx], IndexClass::PoolConstant, "idx {idx}");
        }
        // User + rotating vary within a pool — [22],[23] rotate WITH [9],[10].
        for idx in [1usize, 5, 6, 9, 10, 22, 23] {
            assert_eq!(m[idx], IndexClass::VariesWithinPool, "idx {idx}");
        }
    }

    #[test]
    fn recipient_freshness_validator_rejects_each_bad_condition() {
        let wsol = crate::market_discovery::WSOL_MINT;
        let good_ata = || {
            let mut data = vec![0u8; 165];
            data[0..32].copy_from_slice(Pubkey::from_str(wsol).unwrap().as_ref());
            AccountData {
                owner: Pubkey::from_str(TOKEN_PROGRAM).unwrap(),
                executable: false,
                data,
            }
        };
        let rec = AccountData {
            owner: Pubkey::default(),
            executable: false,
            data: vec![],
        };
        // Valid.
        assert!(
            validate_recipient_freshness(Some(&rec), Some(&good_ata()), wsol, false, true).is_ok()
        );
        // Deployment changed.
        assert_eq!(
            validate_recipient_freshness(Some(&rec), Some(&good_ata()), wsol, true, true),
            Err(FreshnessReject::DeploymentChanged)
        );
        // Outside policy.
        assert_eq!(
            validate_recipient_freshness(Some(&rec), Some(&good_ata()), wsol, false, false),
            Err(FreshnessReject::OutsidePolicy)
        );
        // Missing.
        assert_eq!(
            validate_recipient_freshness(None, Some(&good_ata()), wsol, false, true),
            Err(FreshnessReject::RecipientMissing)
        );
        // Wrong owner program.
        let mut bad = good_ata();
        bad.owner = Pubkey::default();
        assert_eq!(
            validate_recipient_freshness(Some(&rec), Some(&bad), wsol, false, true),
            Err(FreshnessReject::WrongAtaOwnerProgram)
        );
        // Wrong mint.
        let mut wrong_mint = good_ata();
        wrong_mint.data[0..32].copy_from_slice(Pubkey::new_unique().as_ref());
        assert_eq!(
            validate_recipient_freshness(Some(&rec), Some(&wrong_mint), wsol, false, true),
            Err(FreshnessReject::WrongMint)
        );
    }

    #[test]
    fn substitute_touches_only_user_indices() {
        let r = route1();
        let out = substitute_user_accounts(&r.fixtures[0].accounts, "AUTH", "BASE", "QUOTE");
        assert_eq!(out[1].pubkey, "AUTH");
        assert_eq!(out[5].pubkey, "BASE");
        assert_eq!(out[6].pubkey, "QUOTE");
        for (i, (o, orig)) in out.iter().zip(&r.fixtures[0].accounts).enumerate() {
            if ![1, 5, 6].contains(&i) {
                assert_eq!(o.pubkey, orig.pubkey);
            }
        }
    }

    #[test]
    fn route2_is_not_present_as_supported() {
        // Route 2 is UNSUPPORTED_FOR_DIRECT_PARITY — it must not appear as a
        // SUFFICIENT reconstruction fixture set.
        let all = load();
        assert!(all.routes.values().all(|r| r.route != "route2"));
    }

    #[test]
    fn fee_v2_status_string_is_unresolved() {
        assert!(FEE_V2_STATUS.contains("UNRESOLVED"));
        assert_eq!(FEE_V2_INDICES, [19, 21, 22, 23]);
    }
}
