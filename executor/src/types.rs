//! Wire types mirroring the TypeScript monitor's `ArbitrageCycle` JSON
//! (BigInts arrive as decimal strings — parsed here into u64).

use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum DexKind {
    #[serde(rename = "raydium-v4")]
    RaydiumV4,
    #[serde(rename = "orca-whirlpool")]
    OrcaWhirlpool,
}

fn string_u64<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    s.parse::<u64>().map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpportunityHop {
    pub pool: String,
    pub dex: DexKind,
    pub input_mint: String,
    pub output_mint: String,
    #[serde(deserialize_with = "string_u64")]
    pub amount_in: u64,
    #[serde(deserialize_with = "string_u64")]
    pub expected_amount_out: u64,
    #[serde(deserialize_with = "string_u64")]
    pub min_amount_out: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Opportunity {
    pub id: String,
    pub base_mint: String,
    #[serde(default)]
    pub base_symbol: Option<String>,
    pub hops: Vec<OpportunityHop>,
    #[serde(deserialize_with = "string_u64")]
    pub amount_in: u64,
    #[serde(deserialize_with = "string_u64")]
    pub expected_amount_out: u64,
    #[serde(deserialize_with = "string_u64")]
    pub gross_profit: u64,
    #[serde(deserialize_with = "string_u64")]
    pub estimated_cost_in_base: u64,
    #[serde(deserialize_with = "string_u64")]
    pub net_profit: u64,
    pub net_profit_bps: f64,
    #[serde(deserialize_with = "string_u64")]
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
}
