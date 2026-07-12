# Meteora DLMM — verified layout & math (S4 ground truth)

Verified against **mainnet** on 2026-07-12 via the project's RPC. Program:
`LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo` (executable).

Sample pair: `J4cGfY61ZMaBD2niXcfaUD7KsNZiDnjMnJsPJficos8J` — a live
**pump-token/WSOL** DLMM pool (our exact target market), 904 bytes,
bin_step 15, active_id 643 at capture. Fixtures checked into
`monitor/fixtures/meteora/` (raw account bytes, not fabrications).

## LbPair (disc `210b3162b565b10d` = sha256("account:LbPair")[..8])

| Offset | Size | Field | Verified how |
|---|---|---|---|
| 0 | 8 | discriminator | computed + matched |
| 8 | 32 | StaticParameters | values plausible & self-consistent (below) |
| 40 | 32 | VariableParameters | va/vr/index_ref consistent; ts @56 ≈ now |
| 72 | 1 | bump_seed | 255 |
| 73 | 2 | bin_step_seed | — |
| 75 | 1 | pair_type | 3 |
| 76 | 4 | **active_id** (i32) | 643 — inside the fetched bin array; liquidity flips X↔Y exactly there |
| 80 | 2 | **bin_step** (u16) | 15 — price formula byte-exact with it |
| 82 | 1 | status | 0 |
| 88 | 32 | **token_x_mint** | `9cRCn9…pump` (the memecoin) |
| 120 | 32 | **token_y_mint** | **WSOL** |
| 152 | 32 | **reserve_x** | SPL token acct, mint==token_x, owner==pair ✅ |
| 184 | 32 | **reserve_y** | SPL token acct, mint==token_y, owner==pair ✅ |
| 216 | 16 | protocol_fee (x u64, y u64) | plausible (0 / 23_348_355) |

StaticParameters (offset 8): base_factor u16, filter_period u16, decay_period
u16, reduction_factor u16, variable_fee_control u32, max_volatility_accumulator
u32, min_bin_id i32, max_bin_id i32, protocol_share u16, base_fee_power_factor
u8, pad[5]. Sample: 10000 / 30 / 600 / 5000 / 30000 / 350000 / ±23442 / 1000 / 0.

VariableParameters (offset 40): volatility_accumulator u32, volatility_reference
u32, index_reference i32, pad[4], **last_update_timestamp i64 @ offset 56**
(verified: parsed to a unix time ~5 min before capture; offset 52 reads garbage
— it is padding).

### Critical finding (same as Pump AMM)
**WSOL is token Y in this pair** — side must be derived from the mints. Also,
the transaction we sampled touched TWO different DLMM pools; the BinArray
`lb_pair` back-pointer is what distinguishes them (validate it, always).

## BinArray (disc `5c8e5cdc059446b5`, exactly 10 136 bytes = 8+8+8+32+70·144)

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | discriminator |
| 8 | 8 | index (i64) — array covers bins `[index·70, index·70+70)` |
| 16 | 1(+7) | version + padding |
| 24 | 32 | **lb_pair** back-pointer (MUST match the pair) |
| 56 | 70×144 | bins |

Bin (144 B): amount_x u64, amount_y u64, **price u128 (Q64.64)**,
liquidity_supply u128, reward_per_token_stored [2]u128, fee_x_per_token u128,
fee_y_per_token u128, amount_x_in u128, amount_y_in u128.

Observed invariant (fixture): bins **below** active hold Y (WSOL), bins
**above** active hold X — consistent with price = Y per X rising with id.

## Price math — **EXACT (byte-verified)**

`price(id) = (1 + bin_step/10000)^id` in Q64.64, computed with Meteora's
`u128x128_math::pow`: if base ≥ 1.0, work with `u128::MAX / base` and flip an
invert flag; every multiply is floor(`·  >> 64`); invert at the end via
`u128::MAX / result`.

Verified: **byte-identical to the stored `bin.price` for all 140 initialised
bins across two pools with different bin steps (15 and 20).** Naive
floor/ceil/round binary pow does NOT match (off by thousands of ulps) — the
inverse-base algorithm is required.

`bin_array_index(id) = floor(id / 70)` (euclidean, negatives correct).

## Fees — PROVISIONAL until S9
- base_fee_rate = `base_factor · bin_step · 10 · 10^base_fee_power_factor`
  (1e9 scale). Sample: 10000·15·10 = 1_500_000 = **0.15%**.
- variable_fee_rate = `ceil((va · bin_step)² · variable_fee_control / 1e11)`.
- total capped at 1e8 (**10%**).
- volatility: at swap start, if elapsed ≥ filter_period: vr = va·reduction/10⁴
  (or 0 past decay_period), index_ref = active_id; per crossed bin:
  `va = min(vr + |bin−index_ref|·10⁴, max_va)`.
- per bin, exact-in: if the bin is fully drained, input charged =
  `raw_in + ceil(raw_in·rate/(1e9−rate))`; if partial, fee =
  `ceil(in·rate/1e9)` off the top, remainder converted at bin price
  (output floor).

## Swap traversal — PROVISIONAL until S9
- X in → Y out (`swap_for_y`): start at active_id, walk **down**.
- Y in → X out: walk **up**.
- Output conversion: `y = floor(price·x >> 64)`, `x = floor((y << 64)/price)`.
- Bin-drain input bound: ceil variants of the same.
- A missing bin array ⇒ `InsufficientBinCoverage { missing_array_index }` —
  the quote refuses; it never fabricates liquidity.

## Live parity results (2026-07-12, snapshot-ring method)

Method: continuous single-slot snapshots (pair + reserves + 3 bin arrays)
every ~0.9 s; each observed real swap whose `preTokenBalances` equal a stored
snapshot's reserves is quoted through the **Rust** `dlmm_quote_exact_in` (via
`dlmm_quote_cli`) and compared to the actual output:

| tx | direction | in | actual out | rust quote | delta |
|---|---|---|---|---|---|
| 3DDvKEow… | X→Y | 21,000,000 | 61,234,513 | 61,234,512 | **−1** (conservative) |
| 5gTNbQkY… | X→Y | 11,073,244 | 32,288,795 | 32,288,794 | **−1** (conservative) |
| 3nXyLp1v… | X→Y | 404,578,430 | 1,179,721,384 | 1,179,722,063 | **+679 (+0.0006%) OVER** |

## Known gaps (S4b blockers — from Meteora's own `commons/src/quote.rs`)
- **Per-bin LIMIT ORDER fills**: current DLMM bins can carry open limit
  orders (`open_order_amount`, `processed_order_remaining_amount`) that add
  out-side liquidity; our model is MM-liquidity-only. Likely source of the
  +679 crossing overestimate.
- **collect-fee-mode**: some pools charge the fee on INPUT instead of output
  (`fee_on_input` in the official quote path); not modelled.
- Drain-boundary condition aligned to Meteora's strict `>` (fixed).
- Token-2022 transfer-fee interaction; bitmap/extension parsing (discovery).

**Consequence (enforced by policy):** the DLMM quote is NEAR-PARITY, not
exact. Until the full port of Meteora's official off-chain quote passes live
parity with zero overestimates, it must not gate real sizing decisions.
Reference sources vendored for the port: `MeteoraAg/dlmm-sdk/commons/src/`
(quote.rs, extensions/{bin,lb_pair}.rs, math/*).
