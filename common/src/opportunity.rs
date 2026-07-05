//! Wire types for the `arbitrage_opportunities` payload.
//!
//! Serialization mirrors the TypeScript monitor byte-for-byte: camelCase
//! keys, u64 amounts as decimal STRINGS (TS serializes BigInt that way),
//! `netProfitBps` as a JSON number, and `baseSymbol` omitted when absent
//! (TS `JSON.stringify` drops `undefined`). The Rust monitor produces this
//! struct; the executor consumes it — both through this one definition.

use serde::{Deserialize, Serialize};

pub use crate::ix::DexKind;

mod string_u64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        s.parse::<u64>().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpportunityHop {
    pub pool: String,
    pub dex: DexKind,
    pub input_mint: String,
    pub output_mint: String,
    #[serde(with = "string_u64")]
    pub amount_in: u64,
    #[serde(with = "string_u64")]
    pub expected_amount_out: u64,
    #[serde(with = "string_u64")]
    pub min_amount_out: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Opportunity {
    pub id: String,
    pub base_mint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_symbol: Option<String>,
    pub hops: Vec<OpportunityHop>,
    #[serde(with = "string_u64")]
    pub amount_in: u64,
    #[serde(with = "string_u64")]
    pub expected_amount_out: u64,
    #[serde(with = "string_u64")]
    pub gross_profit: u64,
    #[serde(with = "string_u64")]
    pub estimated_cost_in_base: u64,
    #[serde(with = "string_u64")]
    pub net_profit: u64,
    pub net_profit_bps: f64,
    #[serde(with = "string_u64")]
    pub slot: u64,
    pub discovered_at_ms: u64,
}

impl Opportunity {
    pub fn age_ms(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now.saturating_sub(self.discovered_at_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact shape produced by the TS monitor's jsonStringifyBigint.
    const SAMPLE: &str = r#"{
        "id":"a1b2c3d4e5f60708",
        "baseMint":"So11111111111111111111111111111111111111112",
        "baseSymbol":"SOL",
        "hops":[
            {"pool":"HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ","dex":"orca-whirlpool",
             "inputMint":"So11111111111111111111111111111111111111112",
             "outputMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
             "amountIn":"10000000000","expectedAmountOut":"1529000000","minAmountOut":"1525942000"},
            {"pool":"58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2","dex":"raydium-v4",
             "inputMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
             "outputMint":"So11111111111111111111111111111111111111112",
             "amountIn":"1529000000","expectedAmountOut":"10126000000","minAmountOut":"10105748000"}
        ],
        "amountIn":"10000000000",
        "expectedAmountOut":"10126000000",
        "grossProfit":"126000000",
        "estimatedCostInBase":"1105000",
        "netProfit":"124895000",
        "netProfitBps":124,
        "slot":"312345678",
        "discoveredAtMs":1751712345678
    }"#;

    #[test]
    fn parses_monitor_payload() {
        let opp: Opportunity = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(opp.hops.len(), 2);
        assert_eq!(opp.hops[0].dex, DexKind::OrcaWhirlpool);
        assert_eq!(opp.hops[1].dex, DexKind::RaydiumV4);
        assert_eq!(opp.amount_in, 10_000_000_000);
        assert_eq!(opp.gross_profit, 126_000_000);
        assert_eq!(opp.net_profit, 124_895_000);
        assert_eq!(opp.slot, 312_345_678);
        assert_eq!(opp.hops[1].min_amount_out, 10_105_748_000);
    }

    /// Producer-side format: round-trips losslessly and keeps the exact
    /// TS conventions (string amounts, camelCase, numeric bps, no
    /// baseSymbol key when None).
    #[test]
    fn serializes_like_typescript() {
        let opp: Opportunity = serde_json::from_str(SAMPLE).unwrap();
        let json = serde_json::to_value(&opp).unwrap();
        assert_eq!(json["amountIn"], "10000000000"); // string, not number
        assert_eq!(json["netProfitBps"], 124.0); // number, not string
        assert_eq!(json["hops"][0]["dex"], "orca-whirlpool");
        assert_eq!(json["baseSymbol"], "SOL");
        assert_eq!(json["discoveredAtMs"], 1_751_712_345_678u64);

        // Round trip.
        let back: Opportunity = serde_json::from_value(json).unwrap();
        assert_eq!(back.net_profit, opp.net_profit);

        // baseSymbol omitted when None, like TS drops undefined.
        let mut no_sym = opp.clone();
        no_sym.base_symbol = None;
        let v = serde_json::to_value(&no_sym).unwrap();
        assert!(v.get("baseSymbol").is_none());
    }
}
