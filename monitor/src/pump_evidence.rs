//! Pump `sell` fee-v2 account-layout EVIDENCE (S13C slice 1).
//!
//! Loads the persisted evidence artifact
//! (`monitor/fixtures/pump/fee_v2_evidence.json`) — captured from real,
//! successful, DIRECT (top-level, non-CPI) Pump `sell` transactions — and
//! proves, deterministically, exactly which of the 24 accounts are
//! reproducible and which are not.
//!
//! Provenance classes (recorded per account):
//! - `proven_by_pda`         — matches a Rust PDA re-derivation (this module).
//! - `proven_by_pool_field`  — equals a field read from the pool account.
//! - `proven_by_id`          — a fixed program id.
//! - `proven_by_ownership_*` — identified by account owner / executability.
//! - `proven_by_flag`        — the message header's signer flag.
//! - `user_specific*`        — the seller's accounts (substituted at sim time).
//! - `rotating`              — protocol-fee recipient + ATA (vary per tx).
//! - `inferred_*;seeds_undocumented` — the fee-v2 accounts [19,21,22,23]:
//!   consistent per pool and owned by the fee program, but their PDA seeds are
//!   UNDOCUMENTED, so they are NOT considered reproducible from scratch. This
//!   is exactly why the sim harness must CLONE them from a real sell.
//!
//! The tests below fail if anyone later relabels [19,21,22,23] as "proven".

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Evidence {
    pub program_id: String,
    pub fee_program_id: String,
    pub sell_discriminator: String,
    pub data_format: String,
    pub routes: std::collections::BTreeMap<String, RouteEvidence>,
}

#[derive(Debug, Deserialize)]
pub struct RouteEvidence {
    pub pool: String,
    #[serde(default)]
    pub pool_base_mint: Option<String>,
    #[serde(default)]
    pub pool_quote_mint: Option<String>,
    #[serde(default)]
    pub pool_coin_creator: Option<String>,
    pub direct_sells_found: usize,
    #[serde(default)]
    pub sells: Vec<SellEvidence>,
}

#[derive(Debug, Deserialize)]
pub struct SellEvidence {
    pub sig: String,
    pub slot: u64,
    pub data_hex: String,
    pub base_amount_in: u64,
    pub min_quote_out: u64,
    pub accounts: Vec<AccountEvidence>,
}

#[derive(Debug, Deserialize)]
pub struct AccountEvidence {
    pub i: usize,
    pub pubkey: String,
    pub writable: bool,
    pub signer: bool,
    pub owner: Option<String>,
    pub role: String,
    pub provenance: String,
}

pub const EVIDENCE_JSON: &str = include_str!("../fixtures/pump/fee_v2_evidence.json");

pub fn load() -> Evidence {
    serde_json::from_str(EVIDENCE_JSON).expect("fee_v2_evidence.json parses")
}

/// Minimal hex decoder (avoids a `hex` crate dependency).
pub fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim_parity::{
        coin_creator_vault_ata, coin_creator_vault_authority, event_authority, pump_global_config,
        pump_program, PUMP_FEE_PROGRAM_ID, PUMP_PROGRAM_ID,
    };
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;

    /// The fee-v2 accounts we deliberately did NOT resolve to PDA seeds.
    const UNDOCUMENTED_FEE_INDICES: [usize; 4] = [19, 21, 22, 23];

    #[test]
    fn evidence_is_multi_pool_and_direct_only() {
        let e = load();
        assert_eq!(e.program_id, PUMP_PROGRAM_ID);
        assert_eq!(e.fee_program_id, PUMP_FEE_PROGRAM_ID);
        assert_eq!(e.sell_discriminator, "33e685a4017f83ad");
        assert_eq!(
            e.data_format,
            "disc(8)|base_amount_in:u64|min_quote_out:u64"
        );
        // At least two DIFFERENT pools with real direct sells.
        let with_sells = e
            .routes
            .values()
            .filter(|r| r.direct_sells_found > 0)
            .count();
        assert!(
            with_sells >= 2,
            "need ≥2 pools with direct sells, got {with_sells}"
        );
        // At least one pool with several (≥3) sells (evidence depth).
        assert!(
            e.routes.values().any(|r| r.direct_sells_found >= 3),
            "need ≥1 pool with ≥3 direct sells"
        );
    }

    #[test]
    fn every_sell_has_24_accounts_and_the_verified_data_format() {
        let e = load();
        for r in e.routes.values() {
            for s in &r.sells {
                assert_eq!(s.accounts.len(), 24, "sell {} account count", s.sig);
                let bytes = hex_decode(&s.data_hex);
                assert_eq!(bytes.len(), 24, "data must be disc(8)+2×u64");
                assert_eq!(
                    &bytes[0..8],
                    &[0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad]
                );
                // Reconstruct data from semantic fields → byte-exact.
                let mut rebuilt = bytes[0..8].to_vec();
                rebuilt.extend_from_slice(&s.base_amount_in.to_le_bytes());
                rebuilt.extend_from_slice(&s.min_quote_out.to_le_bytes());
                assert_eq!(rebuilt, bytes, "data reconstruction byte-exact ({})", s.sig);
            }
        }
    }

    #[test]
    fn pda_accounts_reproduce_from_pool_fields() {
        let e = load();
        for (name, r) in &e.routes {
            if r.sells.is_empty() {
                continue;
            }
            let quote = Pubkey::from_str(r.pool_quote_mint.as_ref().unwrap()).unwrap();
            let creator = Pubkey::from_str(r.pool_coin_creator.as_ref().unwrap()).unwrap();
            let gc = pump_global_config();
            let ea = event_authority(&pump_program());
            let cc_auth = coin_creator_vault_authority(&creator);
            let cc_ata = coin_creator_vault_ata(&creator, &quote);
            for s in &r.sells {
                let at = |i: usize| Pubkey::from_str(&s.accounts[i].pubkey).unwrap();
                assert_eq!(at(2), gc, "{name} [2] global_config");
                assert_eq!(at(15), ea, "{name} [15] event_authority");
                assert_eq!(at(18), cc_auth, "{name} [18] cc_vault_authority");
                assert_eq!(at(17), cc_ata, "{name} [17] cc_vault_ata");
                // Fixed program ids.
                assert_eq!(s.accounts[16].pubkey, PUMP_PROGRAM_ID);
                assert_eq!(s.accounts[20].pubkey, PUMP_FEE_PROGRAM_ID);
            }
        }
    }

    #[test]
    fn globals_are_identical_across_all_pools() {
        let e = load();
        for idx in [2usize, 15, 16, 20] {
            let mut seen: Option<String> = None;
            for r in e.routes.values() {
                for s in &r.sells {
                    let pk = &s.accounts[idx].pubkey;
                    match &seen {
                        None => seen = Some(pk.clone()),
                        Some(v) => assert_eq!(v, pk, "account [{idx}] must be global"),
                    }
                }
            }
        }
    }

    #[test]
    fn user_accounts_and_rotation_are_flagged_and_fee_v2_not_overclaimed() {
        let e = load();
        for r in e.routes.values() {
            for s in &r.sells {
                // Signer flag only on the user (index 1).
                assert!(s.accounts[1].signer && !s.accounts[0].signer);
                assert!(s.accounts[5].role.starts_with("user"));
                assert!(s.accounts[6].role.starts_with("user"));
                assert!(s.accounts[9].provenance.contains("rotating"));
                assert!(s.accounts[10].provenance.contains("rotating"));
                // HONESTY INVARIANT: the fee-v2 accounts must NOT be labelled
                // as reproducible — cloning is mandatory for them.
                for i in UNDOCUMENTED_FEE_INDICES {
                    let prov = &s.accounts[i].provenance;
                    assert!(
                        prov.contains("undocumented") && !prov.contains("proven"),
                        "fee-v2 account [{i}] must stay undocumented, got `{prov}`"
                    );
                }
            }
        }
    }

    /// Documents the rotation we observed: at least one pool shows ≥2 distinct
    /// protocol-fee recipients at [9]/[10], proving they vary per tx.
    #[test]
    fn rotation_observed_on_at_least_one_pool() {
        let e = load();
        let rotates = e.routes.values().any(|r| {
            let d9: std::collections::BTreeSet<_> = r
                .sells
                .iter()
                .map(|s| s.accounts[9].pubkey.clone())
                .collect();
            d9.len() >= 2
        });
        assert!(rotates, "expected ≥2 distinct fee recipients on some pool");
    }
}
