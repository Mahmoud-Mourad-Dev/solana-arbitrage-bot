//! Forensics input schemas.
//!
//! v1 (`schema_version: 1`) is the original S15B fixture layout, specific to
//! the `meteora-dlmm+orca-whirlpool` family. v2 (`schema_version: 2`)
//! describes an arbitrary venue pair. The v1 loader converts to v2 so the
//! published S15B numbers stay reproducible from the committed fixture.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Wrapped SOL. The one mint every cycle in this codebase is denominated in.
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

// ─────────────────────────── v2 (generic) ───────────────────────────

/// One pool on each venue for the same market.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolPair {
    /// Pool address on `venue_a`.
    pub pool_a: String,
    /// Pool address on `venue_b`.
    pub pool_b: String,
    /// The traded (non-quote) mint.
    pub token_mint: String,
    /// The quote mint used to price non-cycle P&L. When this is WSOL, only
    /// inventory-neutral cycles are priceable and everything else is counted
    /// `Unpriceable` — never estimated.
    pub quote_mint: String,
}

/// An evidence transaction carried over from a v1 input (used by Q1 when
/// present; otherwise Q1 runs on the value-positive events Q4 discovers).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceTx {
    pub sig: String,
    pub slot: u64,
    /// Resolved pool addresses this transaction touched (raw order, may
    /// contain duplicates; consumers dedup/sort).
    pub pools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputV2 {
    pub schema_version: u32,
    #[serde(default)]
    pub description: String,
    /// Venue adapter names — must resolve via `venues::adapter()`.
    pub venue_a: String,
    pub venue_b: String,
    pub pools: Vec<PoolPair>,
    pub slot_min: u64,
    pub slot_max: u64,
    pub window_hours: f64,
    /// Known operator signers (provenance only; the pipeline measures the
    /// whole population, not a wallet list).
    #[serde(default)]
    pub known_signers: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<EvidenceTx>,
}

impl InputV2 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 2 {
            bail!("expected schema_version 2, got {}", self.schema_version);
        }
        if self.pools.is_empty() {
            bail!("no pool pairs");
        }
        if self.slot_min >= self.slot_max {
            bail!("slot_min {} >= slot_max {}", self.slot_min, self.slot_max);
        }
        crate::forensics::venues::adapter(&self.venue_a)
            .with_context(|| format!("unknown venue_a {:?}", self.venue_a))?;
        crate::forensics::venues::adapter(&self.venue_b)
            .with_context(|| format!("unknown venue_b {:?}", self.venue_b))?;
        Ok(())
    }

    /// All pool addresses on side A / side B (deduped, sorted).
    pub fn side_pools(&self) -> (Vec<String>, Vec<String>) {
        let mut a: Vec<String> = self.pools.iter().map(|p| p.pool_a.clone()).collect();
        let mut b: Vec<String> = self.pools.iter().map(|p| p.pool_b.clone()).collect();
        a.sort();
        a.dedup();
        b.sort();
        b.dedup();
        (a, b)
    }

    /// The single quote mint of this input, or an error if pools disagree —
    /// mixed-quote inputs would silently mis-price P&L.
    pub fn quote_mint(&self) -> Result<&str> {
        let q = &self.pools[0].quote_mint;
        for p in &self.pools {
            if &p.quote_mint != q {
                bail!("mixed quote mints in one input: {} vs {}", q, p.quote_mint);
            }
        }
        Ok(q)
    }
}

// ─────────────────────────── v1 (S15B legacy) ───────────────────────────

#[derive(Debug, Deserialize)]
pub struct InputV1 {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub description: String,
    pub family: String,
    pub window_hours: f64,
    pub slot_min: u64,
    pub slot_max: u64,
    pub signers: Vec<String>,
    pub pairs: Vec<PairV1>,
    pub transactions: Vec<TxV1>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PairV1 {
    pub meteora: String,
    pub whirlpool: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TxV1 {
    pub sig: String,
    pub slot: u64,
    pub signer: String,
    pub pools: Vec<PoolRefV1>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PoolRefV1 {
    pub venue: String,
    pub pool: Option<String>,
}

/// USDC — cited from the explicit major-asset allowlist, not restated.
fn usdc() -> &'static str {
    crate::mint_safety::MAJOR_ASSETS
        .iter()
        .find(|(_, sym)| *sym == "USDC")
        .map(|(m, _)| *m)
        .expect("USDC present in MAJOR_ASSETS")
}

/// Convert a v1 input to v2.
///
/// The mints are not present in v1; they are filled from the documented fact
/// that every market in this family is WSOL/USDC (USDC appears in 45/45 of the
/// reconstructed transactions — `docs/forensic-route-recon-s15a.md`). The
/// conversion refuses any other family rather than guessing.
pub fn v1_to_v2(v1: &InputV1) -> Result<InputV2> {
    if v1.family != "meteora-dlmm+orca-whirlpool" {
        bail!(
            "v1 conversion only defined for meteora-dlmm+orca-whirlpool, got {:?}",
            v1.family
        );
    }
    Ok(InputV2 {
        schema_version: 2,
        description: format!("[converted from v1] {}", v1.description),
        venue_a: "meteora-dlmm".into(),
        venue_b: "orca-whirlpool".into(),
        pools: v1
            .pairs
            .iter()
            .map(|p| PoolPair {
                pool_a: p.meteora.clone(),
                pool_b: p.whirlpool.clone(),
                token_mint: WSOL_MINT.into(),
                quote_mint: usdc().into(),
            })
            .collect(),
        slot_min: v1.slot_min,
        slot_max: v1.slot_max,
        window_hours: v1.window_hours,
        known_signers: v1.signers.clone(),
        evidence: v1
            .transactions
            .iter()
            .map(|t| EvidenceTx {
                sig: t.sig.clone(),
                slot: t.slot,
                pools: t.pools.iter().filter_map(|p| p.pool.clone()).collect(),
            })
            .collect(),
    })
}

/// Load an input file, auto-detecting the schema version.
pub fn load_input(json: &str) -> Result<InputV2> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        schema_version: u32,
    }
    let probe: Probe = serde_json::from_str(json).context("input is not JSON")?;
    let v2 = match probe.schema_version {
        0 | 1 => {
            let v1: InputV1 = serde_json::from_str(json).context("parse v1 input")?;
            v1_to_v2(&v1)?
        }
        2 => serde_json::from_str(json).context("parse v2 input")?,
        n => bail!("unsupported schema_version {n}"),
    };
    v2.validate()?;
    Ok(v2)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_FIXTURE: &str = include_str!("../../fixtures/forensics/s15b_input.json");

    #[test]
    fn v1_fixture_loads_and_converts() {
        let v2 = load_input(V1_FIXTURE).unwrap();
        assert_eq!(v2.venue_a, "meteora-dlmm");
        assert_eq!(v2.venue_b, "orca-whirlpool");
        assert_eq!(v2.pools.len(), 6);
        assert_eq!(v2.evidence.len(), 45);
        assert_eq!(v2.known_signers.len(), 4);
        assert_eq!(v2.quote_mint().unwrap(), usdc());
        let (a, b) = v2.side_pools();
        assert_eq!(a.len(), 3, "3 distinct meteora pools");
        assert_eq!(b.len(), 3, "3 distinct whirlpool pools");
    }

    #[test]
    fn v1_conversion_refuses_unknown_family() {
        let mut v1: InputV1 = serde_json::from_str(V1_FIXTURE).unwrap();
        v1.family = "raydium-v4+pump-amm".into();
        assert!(v1_to_v2(&v1).is_err(), "must refuse to guess mints");
    }

    #[test]
    fn mixed_quote_mints_rejected() {
        let mut v2 = load_input(V1_FIXTURE).unwrap();
        v2.pools[1].quote_mint = WSOL_MINT.into();
        assert!(v2.quote_mint().is_err());
    }

    #[test]
    fn v2_roundtrips_through_serde() {
        let v2 = load_input(V1_FIXTURE).unwrap();
        let json = serde_json::to_string_pretty(&v2).unwrap();
        let back = load_input(&json).unwrap();
        assert_eq!(
            serde_json::to_string(&back.pools).unwrap(),
            serde_json::to_string(&v2.pools).unwrap()
        );
        assert_eq!(back.evidence.len(), v2.evidence.len());
    }
}
