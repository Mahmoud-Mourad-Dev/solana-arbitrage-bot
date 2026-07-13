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

## Swap math — verified against real swap EVENTS

The on-chain buy/sell **events** (CPI logs, disc `e445a52e51cb9a1d`) carry
explicit fee fields (`lp_fee`, `protocol_fee`, `coin_creator_fee`, the
in-pool amount `C`, reserves). Decoding them across creator and creator-less
pools is ground truth, not inference. Buy event disc `67f4521f2cf57777`,
sell `3e2f370aa503dc2a`; instruction `buy_exact_quote_in` disc
`c62e1552b4d9e870`.

**Total fee is always 30 bps; only the split shifts, and each component is
ceiled independently (three separate ceils, NOT one 30-bps ceil):**

| pool | lp | protocol | creator |
|---|---|---|---|
| `coin_creator == 0` | 25 | 5 | 0 |
| has creator | 20 | 5 | 5 |

Fees are charged on the QUOTE token.

*SELL — base in (x) → quote out (what the trader receives):*
```
g   = floor(x · Rq / (Rb + x))
out = g − Σⱼ ceil(g · bpsⱼ / 10⁴)      # lp + protocol + creator, each ceiled
```
**17/17 real swaps byte-exact** (creator + creator-less). Note: this is LESS
than the quote-vault delta `g − lp` by the protocol+creator fees — those leave
the vault to third parties, so an earlier balance-delta study that used the
vault delta over-counted the trader's receipt by ~5 bps. Corrected here.

*BUY — quote in (U, user-paid) → base out:*
```
C   = max C such that C + Σⱼ ceil(C · bpsⱼ / 10⁴) ≤ U   # exact fee inversion
out = floor(C · Rb / (Rq + C))
```
**Exact for creator-less pools.** For creator pools this inversion
**overestimates the true output by a few units** (the on-chain per-component
rounding differs in a way not yet pinned) — since overestimating fabricates
profit, creator-pool BUY returns `CreatorBuyUnverified` and is refused. The
pool remains usable as the SELL leg.

**Open precision items:** exact creator-pool BUY rounding; confirm the
creator fee is always 5 bps (all sampled creator pools showed 5, but the
program reads it dynamically — a `fees-v2`/market-cap-tiered pool could
differ); `coin_creator` offset (243-byte layout) still wants a non-zero
cross-check, though `has_creator` behaves correctly on the zero fixture and
on every creator pool sampled by matching event `coin_creator_fee > 0`.
