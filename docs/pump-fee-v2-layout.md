# Pump `sell` fee-v2 account layout — evidence & provenance (S13C slice 1)

Machine-readable evidence: `monitor/fixtures/pump/fee_v2_evidence.json`
(9 direct, successful, top-level Pump `sell` instructions across 4 pools).
Validated deterministically by `monitor/src/pump_evidence.rs` tests.

- Pump program: `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`
- Pump **fees-v2 program** (separate): `pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ`
- `sell` discriminator: `33e685a4017f83ad`
- **Instruction data (24 bytes): `disc(8) | base_amount_in:u64 | min_quote_out:u64`.**
  No hidden fee/tracking fields — proven by byte-exact reconstruction on every
  fixture.

Pools sampled: route1 `5ByL7MZo…` (1 direct sell — low volume), route2
`ETMhxtEN…` (3), route3 `8qDidAKu…` (3), extra `FDrY5i5k…` (2). Protocol-fee
recipients at [9]/[10] were observed rotating (≥2–3 distinct values per pool).

## The 24 accounts

| idx | role | provenance | substitute at sim time? |
|---|---|---|---|
| 0 | pool | pool account id | no |
| 1 | user (signer) | header signer flag | **yes → payer** |
| 2 | global_config | **PDA** `["global_config"]` ✅ | no |
| 3 | base_mint (token) | pool field | no |
| 4 | quote_mint (WSOL) | pool field | no |
| 5 | user_base_ata | ATA(user, base_mint) | **yes → payer's** |
| 6 | user_quote_ata | user-specific (often ephemeral wSOL) | **yes → payer's** |
| 7 | pool_base_vault | pool field | no |
| 8 | pool_quote_vault | pool field | no |
| 9 | protocol_fee_recipient | **rotating** (from global_config) | refresh from a recent tx |
| 10 | protocol_fee_recipient_ata | **rotating** | refresh from a recent tx |
| 11 | base token program | fixed id | no |
| 12 | quote token program | fixed id | no |
| 13 | system program | fixed id | no |
| 14 | ATA program | fixed id | no |
| 15 | event_authority | **PDA** `["__event_authority"]` ✅ | no |
| 16 | pump program | fixed id | no |
| 17 | coin_creator_vault_ata | **PDA/ATA** (of cc_vault_authority, quote) ✅ | no |
| 18 | coin_creator_vault_authority | **PDA** `["creator_vault", coin_creator]` ✅ | no |
| **19** | fee-program global config | owned by fee program; **seeds UNDOCUMENTED** | **clone** |
| **20** | fee program | executable id (proven) | no |
| **21** | fee-program pool account (uninit.) | pool-specific, consistent; **seeds UNDOCUMENTED** | **clone** |
| **22** | fee-program pool fee-state | owned by fee program (208 B); **seeds UNDOCUMENTED** | **clone** |
| **23** | fee-recipient token account | token-owned (165 B); consistent; **seeds UNDOCUMENTED** | **clone** |

## Conclusion classes

- **Proven by PDA re-derivation** (Rust reproduces them from pool fields):
  [2], [15], [17], [18]. ✅
- **Proven by pool field / fixed id / ownership**: [0],[3],[4],[7],[8],[11],
  [12],[13],[14],[16],[20]. ✅
- **User-specific** (substituted with the sim payer): [1],[5],[6].
- **Rotating** (protocol-fee recipient + ATA): [9],[10] — must be refreshed
  from a recent successful tx, never a stale historical recipient.
- **Undocumented (NOT reproducible from scratch)**: [19],[21],[22],[23] — the
  fees-v2 accounts. Consistent per pool and owned by the fee program, but their
  PDA seeds are not published, so they CANNOT be derived. **The sim harness
  must CLONE them verbatim from a real direct sell on the exact target pool.**

## Reproducibility verdict

`PUMP ACCOUNT LAYOUT: PROVISIONALLY RESOLVED VIA CLONE + VALIDATION`

20/24 accounts are reproducible from first principles; 2 are user-substituted;
2 rotate; **4 (fees-v2) require cloning**. Cloning from a real per-pool sell is
therefore mandatory, and the cloned account list must be validated against this
evidence (roles/owners/PDAs) before use — which the later slices enforce.

## Honesty guard

`pump_evidence.rs` tests fail if anyone relabels [19],[21],[22],[23] as
"proven", or if the data-format reconstruction stops being byte-exact.
