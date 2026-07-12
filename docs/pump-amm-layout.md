# PumpSwap AMM — verified layout & math (S3 ground truth)

Everything here was verified against **mainnet** on 2026-07-12 via the project's
RPC, not assumed. Program: `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`
(executable, BPF upgradeable loader).

## Pool account (Anchor, discriminator `f19a6d0411b16dbc` = sha256("account:Pool")[..8])

Sample verified: `HM4BKerYkMLoPjwMv2CkHjkuac3Ajj5hGzCsd19vW84J` (301 bytes).

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 8 | discriminator | `f19a6d0411b16dbc` |
| 8 | 1 | pool_bump | u8 |
| 9 | 2 | index | u16 LE |
| 11 | 32 | creator | Pubkey |
| 43 | 32 | **base_mint** | Pubkey |
| 75 | 32 | **quote_mint** | Pubkey |
| 107 | 32 | lp_mint | Pubkey |
| 139 | 32 | **pool_base_token_account** | base reserve vault |
| 171 | 32 | **pool_quote_token_account** | quote reserve vault |
| 203 | 8 | lp_supply | u64 LE |
| 211 | 32 | coin_creator | Pubkey (all-zero ⇒ no creator fee) — **PROVISIONAL** (offset not yet cross-checked against a non-zero pool) |
| 243 | 58 | reserved/unknown | all-zero in sample |

**Layout verification method (independent):** the parsed `pool_base_token_account`
and `pool_quote_token_account` were fetched as SPL token accounts; their `mint`
fields matched the parsed `base_mint` and `quote_mint` exactly. This can only
hold if all preceding offsets are correct.

### Critical finding: WSOL can be on EITHER side
In the verified sample, **`base_mint == WSOL`** and `quote_mint` is the memecoin
(the opposite of the common assumption). The parser/quote MUST detect the WSOL
side from the mints and never assume base==token / quote==WSOL.

## Reserves
Reserves are **not** stored in the Pool struct — they are the live SPL balances
of `pool_base_token_account` (base_reserve) and `pool_quote_token_account`
(quote_reserve), exactly like Raydium vaults. The registry tracks these via
token-account updates.

## Fees (verified on-chain from GlobalConfig `ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw`)
- lp_fee_bps = **20**
- protocol_fee_bps = **5**
- coin_creator_fee_bps = dynamic (per-pool, applies only when `coin_creator` set)

Base total (no creator) = **25 bps**.

## Instruction discriminators (sha256("global:<name>")[..8])
- buy  = `66063d1201daebea`
- sell = `33e685a4017f83ad`

## Swap math — **PROVISIONAL until S9 simulation parity**
Constant product `base_reserve * quote_reserve = k`. This crate implements the
standard fee-on-input integer AMM:
```
in_after_fee = amount_in * (10_000 - total_fee_bps) / 10_000   (floor)
amount_out   = in_after_fee * reserve_out / (reserve_in + in_after_fee)  (floor)
```
Rounding is floor (never overestimates output). Zero/insufficient reserves ⇒
structured rejection, never a fabricated quote.

**Not yet verified:** whether PumpSwap takes protocol/creator fees as separate
SOL transfers vs. reducing the effective input (which changes k), and the exact
creator-fee handling. Do NOT call this math "exact" until the S9 harness
reconciles it against `simulateTransaction` on real swaps.
