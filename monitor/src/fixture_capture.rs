//! Pure fixture-capture logic (S13C slice 2): transaction classification,
//! signer/writable/origin flag reconstruction, semantic decode, deduplication,
//! deterministic ordering, program-version comparison, and the versioned
//! fixture schema. All PURE over [`crate::sim_client::RawTx`] so it is fully
//! testable without a network.
//!
//! This slice does NOT substitute accounts, mutate amounts, build parity
//! transactions, or simulate anything.

use crate::sim_client::RawTx;
use serde::{Deserialize, Serialize};

pub const FIXTURE_SCHEMA_VERSION: u32 = 1;

/// Minimal base58 decode (avoids a bs58 dependency).
pub fn b58_decode(s: &str) -> Option<Vec<u8>> {
    const ALPH: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut num: Vec<u8> = vec![0];
    for c in s.bytes() {
        let d = ALPH.iter().position(|&x| x == c)?;
        let mut carry = d;
        for byte in num.iter_mut() {
            carry += (*byte as usize) * 58;
            *byte = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            num.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let zeros = s.bytes().take_while(|&b| b == b'1').count();
    num.reverse();
    let mut out = vec![0u8; zeros];
    out.extend(num.into_iter().skip_while(|&b| b == 0));
    // Re-add leading zeros lost by skip_while if the value was all zeros.
    if out.len() < zeros {
        out = vec![0u8; zeros];
    }
    Some(out)
}

/// Which venue instruction we are hunting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Venue {
    PumpSell,
    MeteoraSwap,
}

impl Venue {
    pub fn program_id(&self) -> &'static str {
        match self {
            Venue::PumpSell => crate::sim_parity::PUMP_PROGRAM_ID,
            Venue::MeteoraSwap => crate::meteora_dlmm::DLMM_PROGRAM_ID,
        }
    }
    /// Instruction discriminator (first 8 data bytes).
    pub fn discriminator(&self) -> [u8; 8] {
        match self {
            Venue::PumpSell => [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad],
            Venue::MeteoraSwap => crate::sim_parity::DLMM_SWAP_DISCRIMINATOR,
        }
    }
}

/// How a candidate transaction was classified against a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxClass {
    /// Accepted: the target instruction is a direct top-level call.
    DirectTopLevel,
    /// Target program only appears in inner (CPI) instructions.
    InnerCpi,
    /// Top-level program is some router/aggregator, target is nested.
    AggregatorRouted,
    Failed,
    UnsupportedVersion,
    MissingMetadata,
    NoTargetInstruction,
    PoolMintMismatch,
}

impl TxClass {
    pub fn accepted(&self) -> bool {
        matches!(self, TxClass::DirectTopLevel)
    }
}

/// Where an account came from in a v0 message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyOrigin {
    Static,
    LutWritable,
    LutReadonly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountMetaRec {
    pub index: usize,
    pub pubkey: String,
    pub signer: bool,
    pub writable: bool,
    pub origin: KeyOrigin,
}

/// Reconstruct (signer, writable, origin) for a resolved account index, from
/// the message header + static/LUT partition. This is the exact runtime rule.
pub fn reconstruct_flags(tx: &RawTx, idx: usize) -> (bool, bool, KeyOrigin) {
    let (nsig, nro_signed, nro_unsigned) = (
        tx.header.0 as usize,
        tx.header.1 as usize,
        tx.header.2 as usize,
    );
    let nstatic = tx.static_keys.len();
    let signer = idx < nsig;
    let writable = if idx < nsig {
        idx < nsig - nro_signed
    } else if idx < nstatic {
        idx < nstatic - nro_unsigned
    } else {
        // Loaded addresses: writable ones come first.
        (idx - nstatic) < tx.loaded_writable.len()
    };
    let origin = if idx < nstatic {
        KeyOrigin::Static
    } else if (idx - nstatic) < tx.loaded_writable.len() {
        KeyOrigin::LutWritable
    } else {
        KeyOrigin::LutReadonly
    };
    (signer, writable, origin)
}

/// Semantic fields decoded from the target instruction data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodedFields {
    /// Pump: base_amount_in. Meteora: amount_in.
    pub amount_in: u64,
    /// Pump: min_quote_out. Meteora: min_amount_out.
    pub min_out: u64,
}

/// A captured, validated (or rejected) fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fixture {
    pub schema_version: u32,
    pub route_id: String,
    pub venue: Venue,
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub tx_version: String,
    pub ix_index: usize,
    pub class: TxClass,
    pub program_id: String,
    pub data_b58: String,
    pub decoded: Option<DecodedFields>,
    pub accounts: Vec<AccountMetaRec>,
    pub captured_at_ms: u64,
    /// Filled in by the binary after fetching program accounts.
    pub program_provenance: Option<Vec<ProgramProvenance>>,
    pub validation_status: String,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramProvenance {
    pub program_id: String,
    pub executable: bool,
    pub program_data_address: Option<String>,
    pub fixture_slot: u64,
    pub program_data_slot: Option<u64>,
    /// None when unknown; Some(true) when the current deployment slot is ≤ the
    /// fixture slot (i.e. the fixture was captured under the current program).
    pub current_matches_fixture: Option<bool>,
}

/// Compare a fixture's tx slot against the program's current program-data
/// deployment slot. A fixture captured BEFORE the current deployment ran under
/// an OLDER program and must be flagged.
pub fn program_version_matches(fixture_slot: u64, program_data_slot: Option<u64>) -> Option<bool> {
    program_data_slot.map(|deploy_slot| fixture_slot >= deploy_slot)
}

/// Per-route capture diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureDiagnostics {
    pub signatures_scanned: u64,
    pub transactions_fetched: u64,
    pub rpc_retries: u64,
    pub rpc_failures: u64,
    pub successful_transactions: u64,
    pub direct_target: u64,
    pub cpi_or_aggregator: u64,
    pub failed_transactions: u64,
    pub pool_mint_mismatch: u64,
    pub unsupported_version: u64,
    pub missing_metadata: u64,
    pub duplicates: u64,
    pub accepted_fixtures: u64,
    pub first_scanned_slot: Option<u64>,
    pub last_scanned_slot: Option<u64>,
}

/// Classify + (on acceptance) extract a fixture. `expected_pool` is compared to
/// the pool account of the target instruction; `expected_mint` to the token
/// mint. Pump: pool=accounts[0], token=accounts[3]. Meteora: lb_pair=accounts[0].
pub fn classify(
    tx: &RawTx,
    venue: Venue,
    route_id: &str,
    expected_pool: &str,
    expected_mint: &str,
    direct_only: bool,
    captured_at_ms: u64,
) -> (TxClass, Option<Fixture>) {
    if tx.err.is_some() {
        return (TxClass::Failed, None);
    }
    if tx.version != "0" && tx.version != "legacy" {
        return (TxClass::UnsupportedVersion, None);
    }
    let keys = tx.all_keys();
    let prog = venue.program_id();
    let disc = venue.discriminator();

    // Find a direct top-level target instruction.
    let mut found: Option<(usize, &crate::sim_client::RawIx)> = None;
    for (i, ix) in tx.top_level.iter().enumerate() {
        if keys.get(ix.program_id_index).copied() != Some(prog) {
            continue;
        }
        match b58_decode(&ix.data_b58) {
            Some(d) if d.len() >= 8 && d[0..8] == disc => {
                found = Some((i, ix));
                break;
            }
            _ => {}
        }
    }

    let (ix_index, ix) = match found {
        Some(x) => x,
        None => {
            // Not a direct top-level target. Is the program nested (CPI)?
            let nested = tx.inner_program_ids.iter().any(|p| p == prog);
            if nested {
                let cls = if direct_only {
                    // The top-level program is a router/aggregator.
                    TxClass::AggregatorRouted
                } else {
                    TxClass::InnerCpi
                };
                return (cls, None);
            }
            return (TxClass::NoTargetInstruction, None);
        }
    };

    // Pool/mint match.
    let acc = |k: usize| {
        ix.account_indices
            .get(k)
            .and_then(|&i| keys.get(i))
            .copied()
    };
    let pool_ok = acc(0) == Some(expected_pool);
    let mint_ok = match venue {
        Venue::PumpSell => acc(3) == Some(expected_mint),
        // Meteora: token is x or y; accept if either matches.
        Venue::MeteoraSwap => acc(6) == Some(expected_mint) || acc(7) == Some(expected_mint),
    };
    if !pool_ok || !mint_ok {
        return (TxClass::PoolMintMismatch, None);
    }

    // Decode semantic fields (disc + 2×u64).
    let data = b58_decode(&ix.data_b58).unwrap_or_default();
    let decoded = (data.len() >= 24).then(|| DecodedFields {
        amount_in: u64::from_le_bytes(data[8..16].try_into().unwrap()),
        min_out: u64::from_le_bytes(data[16..24].try_into().unwrap()),
    });

    let accounts = ix
        .account_indices
        .iter()
        .enumerate()
        .map(|(k, &gi)| {
            let (s, w, o) = reconstruct_flags(tx, gi);
            AccountMetaRec {
                index: k,
                pubkey: keys.get(gi).copied().unwrap_or("").to_string(),
                signer: s,
                writable: w,
                origin: o,
            }
        })
        .collect();

    let fx = Fixture {
        schema_version: FIXTURE_SCHEMA_VERSION,
        route_id: route_id.to_string(),
        venue,
        signature: String::new(), // filled by caller (RawTx has no sig)
        slot: tx.slot,
        block_time: tx.block_time,
        tx_version: tx.version.clone(),
        ix_index,
        class: TxClass::DirectTopLevel,
        program_id: prog.to_string(),
        data_b58: ix.data_b58.clone(),
        decoded,
        accounts,
        captured_at_ms,
        program_provenance: None,
        validation_status: "accepted".to_string(),
        rejection_reason: None,
    };
    (TxClass::DirectTopLevel, Some(fx))
}

/// Deduplicate by signature and order deterministically (slot desc, then sig).
pub fn dedup_and_order(mut fixtures: Vec<Fixture>) -> Vec<Fixture> {
    let mut seen = std::collections::BTreeSet::new();
    fixtures.retain(|f| seen.insert(f.signature.clone()));
    fixtures.sort_by(|a, b| {
        b.slot
            .cmp(&a.slot)
            .then_with(|| a.signature.cmp(&b.signature))
    });
    fixtures
}

/// Bounded exponential backoff schedule (ms) for retry attempt `n` (0-based).
pub fn backoff_ms(attempt: u32, base_ms: u64, cap_ms: u64) -> u64 {
    base_ms.saturating_mul(1u64 << attempt.min(16)).min(cap_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim_client::RawIx;

    #[allow(clippy::needless_range_loop, clippy::same_item_push)]
    fn b58(bytes: &[u8]) -> String {
        // encode (test helper) — reverse of b58_decode.
        const A: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
        let zeros = bytes.iter().take_while(|&&b| b == 0).count();
        let mut num = bytes.to_vec();
        let mut out = Vec::new();
        let mut start = 0;
        while start < num.len() {
            let mut rem = 0usize;
            let mut nonzero = start;
            for i in start..num.len() {
                let acc = rem * 256 + num[i] as usize;
                num[i] = (acc / 58) as u8;
                rem = acc % 58;
                if num[i] == 0 && i == nonzero {
                    nonzero += 1;
                }
            }
            out.push(A[rem]);
            start = nonzero;
        }
        for _ in 0..zeros {
            out.push(b'1');
        }
        out.reverse();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn base58_roundtrips_instruction_data() {
        let mut data = vec![0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];
        data.extend_from_slice(&123_456_789u64.to_le_bytes());
        data.extend_from_slice(&42u64.to_le_bytes());
        let enc = b58(&data);
        assert_eq!(b58_decode(&enc).unwrap(), data);
        // Leading-zero preservation.
        assert_eq!(b58_decode(&b58(&[0, 0, 5])).unwrap(), vec![0, 0, 5]);
    }

    fn pump_tx(pool: &str, token: &str, top_prog: &str, amount: u64) -> (RawTx, String) {
        // 24 static keys so account indices resolve; index 0=pool, 3=token.
        let mut keys: Vec<String> = (0..25).map(|i| format!("k{i}")).collect();
        keys[0] = pool.into();
        keys[3] = token.into();
        keys[24] = top_prog.into(); // program id at index 24
        let mut data = crate::sim_parity::PUMP_PROGRAM_ID; // unused
        let _ = &mut data;
        let mut d = vec![0x33u8, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];
        d.extend_from_slice(&amount.to_le_bytes());
        d.extend_from_slice(&7u64.to_le_bytes());
        let ix = RawIx {
            program_id_index: 24,
            account_indices: (0..24).collect(),
            data_b58: b58(&d),
        };
        let tx = RawTx {
            slot: 100,
            block_time: Some(5),
            version: "0".into(),
            err: None,
            header: (1, 0, 3),
            static_keys: keys,
            loaded_writable: vec![],
            loaded_readonly: vec![],
            top_level: vec![ix],
            inner_program_ids: vec![],
        };
        (tx, b58(&d))
    }

    #[test]
    fn accepts_direct_pump_sell_and_decodes() {
        let (tx, _) = pump_tx("POOL", "TOKEN", crate::sim_parity::PUMP_PROGRAM_ID, 999);
        let (cls, fx) = classify(&tx, Venue::PumpSell, "r1", "POOL", "TOKEN", true, 1);
        assert_eq!(cls, TxClass::DirectTopLevel);
        let fx = fx.unwrap();
        assert_eq!(fx.decoded.as_ref().unwrap().amount_in, 999);
        assert_eq!(fx.accounts.len(), 24);
        // Synthetic maps instr position→msg index identically; header nsig=1 ⇒
        // exactly one signer (the account at global index 0). Real layouts are
        // covered by `flag_reconstruction_matches_runtime_partition`.
        assert_eq!(fx.accounts.iter().filter(|a| a.signer).count(), 1);
        assert!(fx.accounts[0].signer);
        assert_eq!(fx.accounts[0].origin, KeyOrigin::Static);
    }

    #[test]
    fn rejects_failed_mismatch_version_and_aggregator() {
        // Failed.
        let (mut tx, _) = pump_tx("POOL", "TOKEN", crate::sim_parity::PUMP_PROGRAM_ID, 1);
        tx.err = Some("InstructionError".into());
        assert_eq!(
            classify(&tx, Venue::PumpSell, "r", "POOL", "TOKEN", true, 0).0,
            TxClass::Failed
        );
        // Pool/mint mismatch.
        let (tx2, _) = pump_tx("OTHER", "TOKEN", crate::sim_parity::PUMP_PROGRAM_ID, 1);
        assert_eq!(
            classify(&tx2, Venue::PumpSell, "r", "POOL", "TOKEN", true, 0).0,
            TxClass::PoolMintMismatch
        );
        // Unsupported version.
        let (mut tx3, _) = pump_tx("POOL", "TOKEN", crate::sim_parity::PUMP_PROGRAM_ID, 1);
        tx3.version = "77".into();
        assert_eq!(
            classify(&tx3, Venue::PumpSell, "r", "POOL", "TOKEN", true, 0).0,
            TxClass::UnsupportedVersion
        );
        // Aggregator-routed: top-level program is a router, pump only in inner.
        let (mut tx4, _) = pump_tx("POOL", "TOKEN", "ROUTER_PROGRAM", 1);
        tx4.top_level[0].program_id_index = 24; // ROUTER at 24
        tx4.inner_program_ids = vec![crate::sim_parity::PUMP_PROGRAM_ID.into()];
        // No top-level pump ix ⇒ nested ⇒ aggregator (direct_only=true).
        assert_eq!(
            classify(&tx4, Venue::PumpSell, "r", "POOL", "TOKEN", true, 0).0,
            TxClass::AggregatorRouted
        );
    }

    #[test]
    fn flag_reconstruction_matches_runtime_partition() {
        let tx = RawTx {
            slot: 1,
            block_time: None,
            version: "0".into(),
            err: None,
            header: (2, 1, 2), // 2 signers (1 ro), then unsigned with 2 readonly
            static_keys: (0..6).map(|i| format!("s{i}")).collect(),
            loaded_writable: vec!["w0".into()],
            loaded_readonly: vec!["r0".into()],
            top_level: vec![],
            inner_program_ids: vec![],
        };
        // idx0: signer+writable; idx1: signer readonly; idx2..3: unsigned writable;
        // idx4,5: unsigned readonly; idx6: LUT writable; idx7: LUT readonly.
        assert_eq!(reconstruct_flags(&tx, 0), (true, true, KeyOrigin::Static));
        assert_eq!(reconstruct_flags(&tx, 1), (true, false, KeyOrigin::Static));
        assert_eq!(reconstruct_flags(&tx, 3), (false, true, KeyOrigin::Static));
        assert_eq!(reconstruct_flags(&tx, 4), (false, false, KeyOrigin::Static));
        assert_eq!(
            reconstruct_flags(&tx, 6),
            (false, true, KeyOrigin::LutWritable)
        );
        assert_eq!(
            reconstruct_flags(&tx, 7),
            (false, false, KeyOrigin::LutReadonly)
        );
    }

    #[test]
    fn dedup_and_deterministic_order() {
        let mk = |sig: &str, slot: u64| Fixture {
            schema_version: 1,
            route_id: "r".into(),
            venue: Venue::PumpSell,
            signature: sig.into(),
            slot,
            block_time: None,
            tx_version: "0".into(),
            ix_index: 0,
            class: TxClass::DirectTopLevel,
            program_id: "p".into(),
            data_b58: "x".into(),
            decoded: None,
            accounts: vec![],
            captured_at_ms: 0,
            program_provenance: None,
            validation_status: "accepted".into(),
            rejection_reason: None,
        };
        let out = dedup_and_order(vec![mk("b", 10), mk("a", 20), mk("b", 10), mk("c", 20)]);
        assert_eq!(out.len(), 3); // dup "b" removed
                                  // slot desc, then sig asc: (a,20),(c,20),(b,10)
        assert_eq!(
            out.iter().map(|f| f.signature.as_str()).collect::<Vec<_>>(),
            vec!["a", "c", "b"]
        );
    }

    #[test]
    fn program_version_mismatch_detection() {
        // Fixture at slot 100; program deployed at slot 90 ⇒ same deployment.
        assert_eq!(program_version_matches(100, Some(90)), Some(true));
        // Program redeployed at slot 150 (after the fixture) ⇒ mismatch.
        assert_eq!(program_version_matches(100, Some(150)), Some(false));
        assert_eq!(program_version_matches(100, None), None);
    }

    #[test]
    fn backoff_is_bounded_exponential() {
        assert_eq!(backoff_ms(0, 200, 5000), 200);
        assert_eq!(backoff_ms(1, 200, 5000), 400);
        assert_eq!(backoff_ms(3, 200, 5000), 1600);
        assert_eq!(backoff_ms(10, 200, 5000), 5000); // capped
    }

    // ─────────────────── dependency / source isolation audit ───────────────────

    /// Strip comment lines so a mention in a doc comment can't trip a grep.
    fn code_only(src: &str) -> String {
        src.lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with("//") && !l.starts_with('*'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn monitor_crate_has_no_submission_or_wallet_deps() {
        // The whole sim-parity toolchain lives in `arb-monitor`, which must not
        // depend on the executor / bot / Jito / QUIC-submission crates.
        let toml = include_str!("../Cargo.toml");
        let deps = toml.split("[dependencies]").nth(1).unwrap_or(toml);
        for forbidden in [
            "arb-executor",
            "arb-bot",
            "jito",
            "solana-quic-client",
            "solana-send",
        ] {
            assert!(
                !deps.contains(forbidden),
                "arb-monitor must not depend on `{forbidden}`"
            );
        }
    }

    #[test]
    fn capture_binary_reaches_no_send_sign_or_keypair_path() {
        // Source-level audit of the capture binary + the modules it uses.
        // Note: we intentionally do NOT grep bare "sign" — `Signature` and
        // `get_signatures` legitimately contain it.
        let sources = [
            include_str!("bin/capture_parity_fixtures.rs"),
            include_str!("sim_client.rs"),
            include_str!("fixture_capture.rs"),
        ];
        let forbidden = [
            "send_transaction",
            "send_and_confirm",
            "send_bundle",
            "sendBundle",
            "request_airdrop",
            "read_keypair_file",
            "Keypair",
            "partial_sign",
            "try_sign",
            "sign_message",
            "JitoClient",
            "crate::executor",
            "arb_executor",
        ];
        for src in sources {
            let code = code_only(src);
            // Drop this test's own forbidden-list lines.
            let code = code
                .split("let forbidden")
                .next()
                .unwrap_or(&code)
                .to_string();
            for needle in forbidden {
                assert!(
                    !code.contains(needle),
                    "forbidden symbol `{needle}` reachable from capture path"
                );
            }
        }
    }
}
