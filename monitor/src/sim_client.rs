//! Isolated chain-access traits for the simulation-parity tooling (S13C
//! slice 2). The surface is deliberately minimal and SPLIT so the
//! fixture-capture command receives only a [`ChainReader`] — it structurally
//! cannot simulate, and NOTHING here can send, sign, bundle, deploy, airdrop,
//! or load a keypair.
//!
//! The concrete [`RpcChainReader`] wraps a read-only Solana RPC client. The
//! pure capture logic in `fixture_capture` consumes the plain domain types
//! below, so it is testable without any network.

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;

/// Minimal decoded account (no lamports needed for capture provenance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountData {
    pub owner: Pubkey,
    pub executable: bool,
    pub data: Vec<u8>,
}

/// One signature-list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigInfo {
    pub signature: String,
    pub slot: u64,
    pub err: bool,
    pub block_time: Option<i64>,
}

/// A raw top-level instruction (indices into the resolved key list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawIx {
    pub program_id_index: usize,
    pub account_indices: Vec<usize>,
    /// Base58 instruction data (as the RPC returns it for `encoding=json`).
    pub data_b58: String,
}

/// A decoded transaction — only the fields capture needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTx {
    pub slot: u64,
    pub block_time: Option<i64>,
    /// "legacy" or "0".
    pub version: String,
    /// Some(msg) if the transaction failed.
    pub err: Option<String>,
    /// (numRequiredSignatures, numReadonlySigned, numReadonlyUnsigned).
    pub header: (u8, u8, u8),
    pub static_keys: Vec<String>,
    pub loaded_writable: Vec<String>,
    pub loaded_readonly: Vec<String>,
    pub top_level: Vec<RawIx>,
    /// Program ids that appear in INNER (CPI) instructions — used to tell an
    /// aggregator-routed / CPI call from a direct top-level one.
    pub inner_program_ids: Vec<String>,
}

impl RawTx {
    /// Resolved key list: static keys, then loaded writable, then readonly —
    /// the order the runtime uses to index instruction accounts.
    pub fn all_keys(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.static_keys.iter().map(String::as_str).collect();
        v.extend(self.loaded_writable.iter().map(String::as_str));
        v.extend(self.loaded_readonly.iter().map(String::as_str));
        v
    }
}

/// Read/discovery operations. This is ALL the fixture-capture command gets.
#[allow(async_fn_in_trait)]
pub trait ChainReader {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<AccountData>>;
    /// Returns (context slot, accounts).
    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<(u64, Vec<Option<AccountData>>)>;
    /// Paginated signatures (newest first); `before` is a cursor signature.
    async fn get_signatures(
        &self,
        address: &Pubkey,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SigInfo>>;
    async fn get_transaction(&self, signature: &str) -> Result<Option<RawTx>>;
}

/// Simulation operations — a SEPARATE trait the capture command never receives.
/// (Used only by the future parity engine, slices 5–7.)
#[allow(async_fn_in_trait)]
pub trait Simulator {
    async fn get_latest_blockhash(&self) -> Result<String>;
    /// Simulate a base64 transaction with sigVerify disabled. Returns the raw
    /// RPC `result` value (the parity engine decodes it). There is NO send path.
    async fn simulate_base64(
        &self,
        tx_b64: &str,
        min_context_slot: u64,
    ) -> Result<serde_json::Value>;
}

// The concrete `ChainReader` lives in the `capture-parity-fixtures` binary,
// where it wraps a read-only RPC client and exposes ONLY these methods — so the
// capture logic (which takes `impl ChainReader`) can never reach a send path.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_keys_orders_static_then_lut() {
        let tx = RawTx {
            slot: 1,
            block_time: None,
            version: "0".into(),
            err: None,
            header: (1, 0, 1),
            static_keys: vec!["A".into(), "B".into()],
            loaded_writable: vec!["W".into()],
            loaded_readonly: vec!["R".into()],
            top_level: vec![],
            inner_program_ids: vec![],
        };
        assert_eq!(tx.all_keys(), vec!["A", "B", "W", "R"]);
    }

    /// The capture command must be handed a `ChainReader` only. This compiles
    /// a generic that accepts a ChainReader but has no way to simulate — a
    /// type-level proof the discovery path can't reach simulation.
    #[test]
    fn chain_reader_bound_excludes_simulation() {
        fn discovery_only<R: ChainReader>(_r: &R) {}
        // (Simulator is a distinct trait; discovery_only cannot call it.)
        let _ = discovery_only::<NoopReader>;
    }

    struct NoopReader;
    impl ChainReader for NoopReader {
        async fn get_account(&self, _p: &Pubkey) -> Result<Option<AccountData>> {
            Ok(None)
        }
        async fn get_multiple_accounts(
            &self,
            _p: &[Pubkey],
        ) -> Result<(u64, Vec<Option<AccountData>>)> {
            Ok((0, vec![]))
        }
        async fn get_signatures(
            &self,
            _a: &Pubkey,
            _b: Option<&str>,
            _l: usize,
        ) -> Result<Vec<SigInfo>> {
            Ok(vec![])
        }
        async fn get_transaction(&self, _s: &str) -> Result<Option<RawTx>> {
            Ok(None)
        }
    }
}
