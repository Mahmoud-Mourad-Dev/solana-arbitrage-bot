//! Dynamic Pump∩Meteora market discovery (S5).
//!
//! Finds tokens with WSOL-paired liquidity on BOTH PumpSwap AMM and Meteora
//! DLMM, screens mint safety (Token-2022 extensions, authorities), and
//! persists a restart cache. The RPC orchestration lives in the
//! `discover-markets` binary; everything here is pure and unit-tested.
//!
//! Funnel: GPA universe → token intersection → structural validation
//! (decoders + status + fee-schedule verification) → WSOL liquidity floor →
//! mint safety screen → ranked cache.
//!
//! Safety policy (reject, never guess): mint or freeze authority present;
//! Token-2022 transfer fee > 0; transfer hook; permanent delegate;
//! non-transferable; default-frozen; pausable; unknown/unclassified
//! extensions. Metadata-class extensions are allowed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

// ───────────────────────── mint safety screen ─────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintParseError {
    TooShort { len: usize },
    NotInitialized,
    NotAMintAccount,
    MalformedTlv { offset: usize },
}

/// Everything the strategy needs to know about a mint to decide if it is
/// safe to trade. Parsed from raw account bytes; `token_2022` comes from the
/// account owner (caller supplies it — ownership is chain-verified upstream).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintSafety {
    pub token_2022: bool,
    pub decimals: u8,
    pub mint_authority: bool,
    pub freeze_authority: bool,
    /// Max of older/newer TransferFeeConfig bps (0 when absent).
    pub transfer_fee_bps: u16,
    pub transfer_hook: bool,
    pub permanent_delegate: bool,
    pub non_transferable: bool,
    pub default_account_frozen: bool,
    pub pausable: bool,
    /// First extension type we do not have a safety classification for.
    pub unknown_extension: Option<u16>,
}

impl MintSafety {
    /// The trade/no-trade verdict. Conservative: anything that could freeze,
    /// tax, hook, or confiscate balances — or that we can't classify — is out.
    pub fn is_safe(&self) -> bool {
        !self.mint_authority
            && !self.freeze_authority
            && self.transfer_fee_bps == 0
            && !self.transfer_hook
            && !self.permanent_delegate
            && !self.non_transferable
            && !self.default_account_frozen
            && !self.pausable
            && self.unknown_extension.is_none()
    }

    /// Human-readable reasons (for the discovery report).
    pub fn reject_reasons(&self) -> Vec<&'static str> {
        let mut r = Vec::new();
        if self.mint_authority {
            r.push("mint-authority-present");
        }
        if self.freeze_authority {
            r.push("freeze-authority-present");
        }
        if self.transfer_fee_bps > 0 {
            r.push("transfer-fee");
        }
        if self.transfer_hook {
            r.push("transfer-hook");
        }
        if self.permanent_delegate {
            r.push("permanent-delegate");
        }
        if self.non_transferable {
            r.push("non-transferable");
        }
        if self.default_account_frozen {
            r.push("default-frozen");
        }
        if self.pausable {
            r.push("pausable");
        }
        if self.unknown_extension.is_some() {
            r.push("unknown-extension");
        }
        r
    }
}

const SPL_MINT_LEN: usize = 82;
const T22_ACCOUNT_TYPE_OFFSET: usize = 165;
const T22_TLV_START: usize = 166;

/// Parse an SPL Token or Token-2022 mint account into a safety verdict.
/// Verified against real fixtures: a classic 82-byte pump mint and a 401-byte
/// Token-2022 pump mint (MetadataPointer + TokenMetadata TLV @166).
pub fn parse_mint_safety(data: &[u8], token_2022: bool) -> Result<MintSafety, MintParseError> {
    if data.len() < SPL_MINT_LEN {
        return Err(MintParseError::TooShort { len: data.len() });
    }
    // COption<Pubkey> tags are u32 LE at 0 (mint auth) and 46 (freeze auth).
    let mint_authority = u32::from_le_bytes(data[0..4].try_into().unwrap()) == 1;
    let decimals = data[44];
    if data[45] != 1 {
        return Err(MintParseError::NotInitialized);
    }
    let freeze_authority = u32::from_le_bytes(data[46..50].try_into().unwrap()) == 1;

    let mut s = MintSafety {
        token_2022,
        decimals,
        mint_authority,
        freeze_authority,
        ..Default::default()
    };

    if !token_2022 || data.len() <= T22_ACCOUNT_TYPE_OFFSET {
        return Ok(s);
    }
    // Token-2022 with extensions: account type byte must say Mint (1).
    if data[T22_ACCOUNT_TYPE_OFFSET] != 1 {
        return Err(MintParseError::NotAMintAccount);
    }
    let mut o = T22_TLV_START;
    while o + 4 <= data.len() {
        let ext_type = u16::from_le_bytes([data[o], data[o + 1]]);
        let len = u16::from_le_bytes([data[o + 2], data[o + 3]]) as usize;
        if ext_type == 0 {
            break; // Uninitialized padding = end of TLV
        }
        let body = o + 4;
        if body + len > data.len() {
            return Err(MintParseError::MalformedTlv { offset: o });
        }
        match ext_type {
            // TransferFeeConfig: older bps @72+16=88? layout: cfg_auth(32) +
            // withdraw_auth(32) + withheld(u64) + older{epoch,max,bps} +
            // newer{epoch,max,bps} → bps at 88..90 and 106..108.
            1 => {
                if len >= 108 {
                    let older = u16::from_le_bytes([data[body + 88], data[body + 89]]);
                    let newer = u16::from_le_bytes([data[body + 106], data[body + 107]]);
                    s.transfer_fee_bps = older.max(newer);
                } else {
                    return Err(MintParseError::MalformedTlv { offset: o });
                }
            }
            6 => {
                // DefaultAccountState: 1 byte, 2 = Frozen.
                if len >= 1 && data[body] == 2 {
                    s.default_account_frozen = true;
                }
            }
            9 => s.non_transferable = true,
            12 => s.permanent_delegate = true,
            14 => s.transfer_hook = true,
            26 => s.pausable = true,
            // Known-safe / irrelevant-to-swaps extensions:
            //   3 MintCloseAuthority (mint closable only at 0 supply)
            //   4 ConfidentialTransferMint, 16/17 confidential fee configs
            //  10 InterestBearingConfig (UI-only), 25 ScaledUiAmount (UI-only)
            //  18 MetadataPointer, 19 TokenMetadata, 20..=23 group/member
            3 | 4 | 10 | 16 | 17 | 18 | 19 | 20 | 21 | 22 | 23 | 25 => {}
            other => {
                if s.unknown_extension.is_none() {
                    s.unknown_extension = Some(other);
                }
            }
        }
        o = body + len;
    }
    Ok(s)
}

// ───────────────────────── intersection ─────────────────────────

/// Tokens present in BOTH maps (mint → pool addresses), WSOL excluded.
/// Deterministic order (sorted by mint) for reproducible runs.
pub fn intersect_tokens(
    pump_by_token: &HashMap<String, Vec<String>>,
    dlmm_by_token: &HashMap<String, Vec<String>>,
) -> Vec<(String, Vec<String>, Vec<String>)> {
    let mut out: Vec<_> = pump_by_token
        .iter()
        .filter(|(mint, _)| mint.as_str() != WSOL_MINT)
        .filter_map(|(mint, pumps)| {
            dlmm_by_token
                .get(mint)
                .map(|dlmms| (mint.clone(), pumps.clone(), dlmms.clone()))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ───────────────────────── cache schema ─────────────────────────

pub const CACHE_VERSION: u32 = 1;

/// One tradable (or tracked-but-refused) token market on both venues.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveredMarket {
    pub token_mint: String,
    pub decimals: u8,
    pub token_2022: bool,

    // PumpSwap side
    pub pump_pool: String,
    pub pump_base_is_wsol: bool,
    pub pump_base_vault: String,
    pub pump_quote_vault: String,
    /// Empirically-verified fee schedule applies (no coin creator).
    pub pump_fee_verified: bool,
    pub pump_wsol_reserve: u64,

    // Meteora DLMM side
    pub dlmm_pair: String,
    pub dlmm_x_is_wsol: bool,
    pub dlmm_reserve_x: String,
    pub dlmm_reserve_y: String,
    pub dlmm_bin_step: u16,
    pub dlmm_wsol_reserve: u64,

    // Screening
    pub safety: MintSafety,
    pub safe: bool,
    /// Ranking key: min(pump_wsol_reserve, dlmm_wsol_reserve) — arb capacity
    /// is bounded by the thinner side.
    pub rank_lamports: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveryCache {
    pub version: u32,
    pub generated_at_ms: u64,
    pub rpc_slot: u64,
    /// Funnel counters, for the report and for drift monitoring.
    pub stats: DiscoveryStats,
    pub markets: Vec<DiscoveredMarket>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiscoveryStats {
    pub pump_wsol_pools: usize,
    pub dlmm_wsol_pairs: usize,
    pub tokens_intersecting: usize,
    pub structurally_valid: usize,
    pub pump_fee_unverified: usize,
    pub above_liquidity_floor: usize,
    pub safe: usize,
    pub rejected_unsafe: usize,
}

impl DiscoveryCache {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("cache serializes")
    }

    /// Load + version check. A version mismatch is a miss, not an error —
    /// the caller re-discovers from scratch.
    pub fn from_json(s: &str) -> Option<DiscoveryCache> {
        let c: DiscoveryCache = serde_json::from_str(s).ok()?;
        (c.version == CACHE_VERSION).then_some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real mainnet fixtures (docs/meteora-dlmm-layout.md):
    const MINT_T22: &[u8] = include_bytes!("../fixtures/meteora/mint_9cRCn9_token2022.bin");
    const MINT_SPL: &[u8] = include_bytes!("../fixtures/meteora/mint_2ux9p7_spl.bin");

    #[test]
    fn parses_real_token2022_pump_mint_as_safe() {
        let s = parse_mint_safety(MINT_T22, true).unwrap();
        assert!(s.token_2022);
        assert_eq!(s.decimals, 6);
        // Chain ground truth: no authorities, only metadata extensions.
        assert!(!s.mint_authority);
        assert!(!s.freeze_authority);
        assert_eq!(s.transfer_fee_bps, 0);
        assert!(!s.transfer_hook && !s.permanent_delegate && !s.pausable);
        assert_eq!(s.unknown_extension, None);
        assert!(s.is_safe(), "reasons: {:?}", s.reject_reasons());
    }

    #[test]
    fn parses_real_classic_spl_pump_mint_as_safe() {
        let s = parse_mint_safety(MINT_SPL, false).unwrap();
        assert!(!s.token_2022);
        assert!(s.is_safe(), "reasons: {:?}", s.reject_reasons());
    }

    #[test]
    fn rejects_dangerous_extensions() {
        // Take the real T22 mint and splice in a hostile TLV stream.
        let mut d = MINT_T22[..T22_TLV_START].to_vec();
        // TransferFeeConfig (type 1, len 108) with newer bps = 300.
        let mut tf = vec![0u8; 108];
        tf[106..108].copy_from_slice(&300u16.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&(108u16).to_le_bytes());
        d.extend_from_slice(&tf);
        // TransferHook (type 14, len 64).
        d.extend_from_slice(&14u16.to_le_bytes());
        d.extend_from_slice(&(64u16).to_le_bytes());
        d.extend_from_slice(&[0u8; 64]);
        // PermanentDelegate (type 12, len 32).
        d.extend_from_slice(&12u16.to_le_bytes());
        d.extend_from_slice(&(32u16).to_le_bytes());
        d.extend_from_slice(&[0u8; 32]);
        // DefaultAccountState frozen (type 6, len 1, state=2).
        d.extend_from_slice(&6u16.to_le_bytes());
        d.extend_from_slice(&(1u16).to_le_bytes());
        d.push(2);
        let s = parse_mint_safety(&d, true).unwrap();
        assert_eq!(s.transfer_fee_bps, 300);
        assert!(s.transfer_hook);
        assert!(s.permanent_delegate);
        assert!(s.default_account_frozen);
        assert!(!s.is_safe());
        assert!(s.reject_reasons().contains(&"transfer-fee"));
    }

    #[test]
    fn rejects_unknown_extension_and_authorities() {
        let mut d = MINT_T22[..T22_TLV_START].to_vec();
        d.extend_from_slice(&999u16.to_le_bytes());
        d.extend_from_slice(&(4u16).to_le_bytes());
        d.extend_from_slice(&[0u8; 4]);
        let s = parse_mint_safety(&d, true).unwrap();
        assert_eq!(s.unknown_extension, Some(999));
        assert!(!s.is_safe());

        // Authorities present ⇒ unsafe (splice tags into the classic mint).
        let mut m = MINT_SPL.to_vec();
        m[0..4].copy_from_slice(&1u32.to_le_bytes()); // mint authority Some
        let s = parse_mint_safety(&m, false).unwrap();
        assert!(s.mint_authority && !s.is_safe());
        let mut m2 = MINT_SPL.to_vec();
        m2[46..50].copy_from_slice(&1u32.to_le_bytes()); // freeze authority Some
        let s2 = parse_mint_safety(&m2, false).unwrap();
        assert!(s2.freeze_authority && !s2.is_safe());
    }

    #[test]
    fn rejects_malformed() {
        assert!(matches!(
            parse_mint_safety(&[0u8; 10], false),
            Err(MintParseError::TooShort { .. })
        ));
        let mut bad = MINT_SPL.to_vec();
        bad[45] = 0; // not initialised
        assert_eq!(
            parse_mint_safety(&bad, false),
            Err(MintParseError::NotInitialized)
        );
        // TLV that claims more bytes than exist.
        let mut d = MINT_T22[..T22_TLV_START].to_vec();
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&(200u16).to_le_bytes());
        d.extend_from_slice(&[0u8; 10]);
        assert!(matches!(
            parse_mint_safety(&d, true),
            Err(MintParseError::MalformedTlv { .. })
        ));
    }

    #[test]
    fn intersection_is_correct_and_deterministic() {
        let mut pump = HashMap::new();
        pump.insert("tokA".into(), vec!["p1".into()]);
        pump.insert("tokB".into(), vec!["p2".into(), "p3".into()]);
        pump.insert(WSOL_MINT.into(), vec!["px".into()]);
        pump.insert("tokC".into(), vec!["p4".into()]);
        let mut dlmm = HashMap::new();
        dlmm.insert("tokB".into(), vec!["d1".into()]);
        dlmm.insert("tokA".into(), vec!["d2".into(), "d3".into()]);
        dlmm.insert(WSOL_MINT.into(), vec!["dx".into()]);
        dlmm.insert("tokD".into(), vec!["d4".into()]);
        let out = intersect_tokens(&pump, &dlmm);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "tokA"); // sorted
        assert_eq!(out[1].0, "tokB");
        assert_eq!(out[1].1, vec!["p2".to_string(), "p3".to_string()]);
        // WSOL itself and one-sided tokens excluded.
        assert!(!out
            .iter()
            .any(|(m, _, _)| m == WSOL_MINT || m == "tokC" || m == "tokD"));
    }

    #[test]
    fn cache_round_trips_and_rejects_wrong_version() {
        let cache = DiscoveryCache {
            version: CACHE_VERSION,
            generated_at_ms: 123,
            rpc_slot: 456,
            stats: DiscoveryStats {
                tokens_intersecting: 2,
                ..Default::default()
            },
            markets: vec![DiscoveredMarket {
                token_mint: "tok".into(),
                decimals: 6,
                token_2022: true,
                pump_pool: "pp".into(),
                pump_base_is_wsol: true,
                pump_base_vault: "bv".into(),
                pump_quote_vault: "qv".into(),
                pump_fee_verified: true,
                pump_wsol_reserve: 1_000,
                dlmm_pair: "dp".into(),
                dlmm_x_is_wsol: false,
                dlmm_reserve_x: "rx".into(),
                dlmm_reserve_y: "ry".into(),
                dlmm_bin_step: 15,
                dlmm_wsol_reserve: 2_000,
                safety: MintSafety::default(),
                safe: true,
                rank_lamports: 1_000,
            }],
        };
        let json = cache.to_json();
        let back = DiscoveryCache::from_json(&json).expect("round trip");
        assert_eq!(back, cache);
        // Wrong version ⇒ miss (forces re-discovery), not a crash.
        let old = json.replace(&format!("\"version\": {CACHE_VERSION}"), "\"version\": 0");
        assert!(DiscoveryCache::from_json(&old).is_none());
    }
}
