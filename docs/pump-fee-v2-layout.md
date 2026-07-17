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

## Slice-3 correction — [22] and [23] ROTATE (revises the slice-1 table)

The slice-1 evidence had only **1** direct sell for route1, which made [22]
and [23] look pool-constant. With **3 distinct-seller fixtures per pool**
(slice 3), the truth emerged: **[22] and [23] rotate WITH the protocol-fee
recipient [9]/[10]** — in route1's third sell, [9], [22], and [23] all change
together. So the coherent rotating set is **[9], [10], [22], [23]**, and a
cloned recipient must copy all four from the SAME source transaction and
refresh them together. [21] remains pool-constant; [19]/[20] remain global.
This vindicates the ≥3-distinct-seller requirement — a single sample would
have mis-fixed [22]/[23].

Corrected index classes (verified in `pump_reconstruct.rs`):
- global: [2],[11],[12],[13],[14],[15],[16],[19],[20]
- pool-specific (constant per pool): [0],[3],[4],[7],[8],[17],[18],[21]
- rotating (per recipient): [9],[10],[22],[23]
- user-specific: [1],[5],[6]

## Honesty guard

`pump_evidence.rs` and `pump_reconstruct.rs` tests fail if anyone relabels
[19],[21],[22],[23] as derivable/proven, if the data reconstruction stops
being byte-exact, or if [22]/[23] are treated as pool-constant.

## Slice-6 — direct SELL simulation parity (S13C)

Binary: `sim-pump-sell` (MODE=simulate only; no sign/send/Jito/keypair). Per
route it captures a FRESH recent successful direct top-level Pump `sell`,
reconstructs it byte-exact, clones the coherent rotating set [9,10,22,23] +
fee-v2 accounts from that same tx, substitutes ONLY [1]/[5]/[6] to a current
token holder, and simulates (`sigVerify=false`) inside a tight reserve bracket
(fetch vaults+pool → simulate at `minContextSlot` → re-fetch; a sample counts
only if the bracket is unchanged, so the sim provably used those reserves).

**What is PROVEN (both routes):**

- Direct top-level Pump `sell` ENTERS and COMPLETES after substitution.
- Byte-exact reconstruction; 24 accounts; [0]=pool, [3]=mint.
- The coherent rotating fee set [9,10,22,23] (same source tx) is ACCEPTED.
- Account substitution is viable: `base(token) delta == amount_in` exactly, and
  WSOL is credited to the substituted [6].
- Same-state (clean-bracket) guard holds.
- Negatives fail for their own reasons: wrong fee-v2 [19] → 3007
  AccountOwnedByWrongProgram (`fee_config`); mixed rotating [9] → 2015
  ConstraintTokenOwner (`protocol_fee_recipient_token_account`); wrong base-acct
  mint → IncorrectProgramId; impossible min_out → 6004 ExceededSlippage
  (`sell.rs:170`); insufficient balance → token `0x1` insufficient funds.

**What FAILS — the quote fee rate (verdict `PUMP QUOTE MISMATCH`):**

The fee-less CPMM **gross is exact** (input side exact, clean-bracket reserves),
but the local quote's **fee rate is stale for fee-v2 pools**. Measured real
total fee (constant across two amounts each, clean brackets):

| route | pool | model fee | REAL fee (measured) | quote gap |
|---|---|---|---|---|
| 1 | `5ByL7MZo…` | 30 bps | **75 bps** | ≈45 bps |
| 3 | `8qDidAKu…` | 30 bps | **95 bps** | ≈65 bps |

`pump_amm::fee_split` hardcodes 30 bps total (`PROTOCOL_FEE_BPS=5` + LP/creator
= 30), which matched the pre-fee-v2 S9 sample (29/29). These two routes are
**fee-v2 pools with PER-POOL rates** (75 vs 95 bps) that the quote engine does
not read — so local WSOL out overestimates the simulated delta by the fee gap
(route1 ≈45 bps, route3 ≈65 bps). The per-pool rate almost certainly lives in
the undocumented fee-v2 `fee_config` [19]; decoding it was NOT attempted here
(out of slice scope, undocumented layout). **A fee-v2 rate model is the required
follow-up before Pump legs can be trusted for profit math.**

Verdict per route: `PUMP QUOTE MISMATCH` (root cause: fee-v2 per-pool fee rate,
not a reconstruction/substitution/harness failure).
