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

## Live parity results

Method (snapshot-ring): continuous single-slot snapshots (pair + reserves +
bin arrays) every ~0.9 s; each observed real swap whose `preTokenBalances`
equal a stored snapshot's reserves is quoted through the **Rust**
`dlmm_quote_exact_in` (via `dlmm_quote_cli`) and compared to the actual
output.

**Round 1 (classic MM-only model):** two single-bin fills −1 unit; one
bin-crossing fill **+679 OVERestimate** → model rejected.

Root cause (from Meteora's own `commons/src/quote.rs` + the official IDL
`idls/dlmm.json`): the current DLMM has **per-bin limit orders**
(`open_order_amount`, `processed_order_remaining_amount`,
`limit_order_ask_side` — inside the same 144-byte Bin) and
**collect-fee-mode** (`InputOnly` / `OnlyY`, i.e. fee side depends on pool
AND direction). Our fixture pool is `function_type=LimitOrder`,
`collect_fee_mode=OnlyY`, `pair_type=PermissionlessV2`, and its X mint is
**Token-2022**.

**Round 2 (S4b: faithful port of the official `quote_exact_in` — fill layers
MM → processed orders → open orders, fee-mode aware, bitmap-based array
traversal, official volatility updates):**

| tx | direction | in | actual out | rust quote | verdict |
|---|---|---|---|---|---|
| 3aPMsWR9… | X→Y | 133,794 | 386,063 | 386,063 | **EXACT** |
| 4NxWhj5c… | Y→X | 1,288,416,783 | 445,173,279 | 445,173,279 | **EXACT** |
| 4dMqun99… | Y→X | 855,218,732 | 295,494,946 | 295,494,946 | **EXACT** |
| zXxZFjS4… | Y→X | 1,125,791,688 | 388,983,272 | 388,983,272 | **EXACT** |
| 3BFLgUaw… | Y→X | 50,958,710 | 17,580,743 | 17,580,743 | **EXACT** |
| 2PMgoBWF… | Y→X | 1,707,400,184 | 589,052,695 | 589,052,695 | **EXACT** |

**6/6 exact, both directions, zero overestimates** — on a live pool
exercising limit orders, OnlyY fee mode, and a Token-2022 X mint.

## Remaining screening duties (discovery-time, not quote-time)
- **Token-2022 transfer fees are NOT modelled in the quote**: discovery must
  parse the mint and reject (or model) mints with a non-zero transfer fee.
  (Pump-suffix mints observed so far have none — verify per mint.)
- Bitmap **extension** account (array indices outside [-512, 511]) is not
  parsed; such pools are refused via `next_array_with_liquidity → None`.
- `Permission` / `CustomizablePermissionless` pairs are refused
  (activation-point gating not carried); `Permissionless`/`PermissionlessV2`
  quoted.
- Larger parity sample (incl. multi-array crossings and an `InputOnly` pool)
  should accumulate in S9 before live eligibility (Gate 2).

## `swap2` instruction layout — reconstruction evidence (S13C slice 4)

Machine-readable evidence: `monitor/fixtures/meteora/swap2_cpi_fixtures.json`.
Validated deterministically by `monitor/src/meteora_reconstruct.rs` tests.

**HONESTY FINDING.** There are **no direct top-level Meteora swaps** on the
supported pairs — every swap is Jupiter/CPI-routed. The captured fixtures are
**CPI-exposed** `swap2` instructions (they expose the exact DLMM instruction +
account metas) but they do **not** satisfy the three-DIRECT-fixture bar. Route 3
(`DdZuEHGSH9LAte28K8SqeewcKQ96k6fXgj7zuWHqNWkv`) has **no** Meteora fixtures.

- Variant: `swap2`, discriminator `414b3f4ceb5b5b88`. (`swap` v1 disc
  `f8c69e91e17587c8` is rejected as not-swap2.)
- Data (28 bytes): `disc(8) | amount_in:u64 | min_amount_out:u64 |
  remaining_accounts_info`. The trailing `remaining_accounts_info` was the empty
  encoding `00000000` in **every** observed fixture. Reconstructed byte-exact;
  the tail is preserved verbatim, never guessed.
- Accounts: 16 fixed (IDL order) + N trailing bin arrays (1–3 observed).

| idx | role | provenance |
|---|---|---|
| 0 | lb_pair | route pair |
| 1 | bin_array_bitmap_extension | **PDA** `["bitmap", pair]` when present, else program-id None-sentinel — both proven against real fixtures |
| 8 | oracle | **PDA** `["oracle", pair]` ✅ (equals the pair's stored oracle) |
| 9 | host_fee_in | program-id None-sentinel |
| 11 | token_x_program | Token-2022 (`Tokenz…`) — matches pair flag |
| 12 | token_y_program | SPL Token (`Tokenkeg…`) — WSOL |
| 14 | event_authority | **PDA** `["__event_authority"]` ✅ |
| 16+ | bin arrays | **PDA** `["bin_array", pair, index_le]`; each must belong to THIS pair; indices strictly monotonic in the traversal direction (descending observed = price-down/sell) |

Negative tests reject: wrong variant, non-byte-exact data, wrong oracle,
foreign bin array, and non-monotonic bin-array order. Token-2022 is present on
token_x but no transfer-fee modelling is asserted here (screening duty, above).

## Direct-call simulation — Route 1 (S13C slice 5)

Binary: `sim-meteora-route1` (MODE=simulate only; no sign/send/Jito/keypair).
Privilege audit: `monitor/src/meteora_direct_call.rs`; direct swap2 builder:
`sim_parity::build_dlmm_swap2_ix`.

**Privilege audit (Stage 5.0).** Of the 16 fixed accounts, the IDL declares
exactly ONE signer — `user` [10] — and it is user-substitutable. In the three
CPI fixtures the authority was a real top-level signer once (an ordinary wallet)
and a Jupiter PDA twice (caller-PDA signing); either way a direct call supplies
our own authority, so no non-replaceable caller-PDA signer exists ⇒
`PrivilegesResolvedViable`. Recorded tier-3 (IDL-inferred, not proven from inner
metadata) privilege deltas: source marks [10] writable (fee payer) though the
IDL is readonly; source marks [15] program writable. Simulation resolved these —
building `user` as `signer, readonly` executes cleanly.

**Direct top-level simulation (Stage 5.3).** A public wallet self-wraps 0.1 SOL
(wSOL ATA create + native transfer + SyncNative), creates the Token-2022 dest
ATA, and calls `swap2` directly (WSOL→token, Y→X walk-up). Result: the Meteora
program is ENTERED and the swap COMPLETES; `~71.9k` CU; tx ~907 B / 22 accounts;
**local Rust quote == simulated destination-token delta EXACTLY (0 abs, 0 bps)**;
same-state guard held (snapshot hash unchanged across the sim). Token-2022 mint
(`FeMbDoX7…pump`) has only metadata-pointer/token-metadata extensions — no
transfer fee/hook — so gross == net and no extra remaining accounts are needed.

Negative controls (all fail for their own DLMM reason, not a layout error):
foreign-pair bin array → 3007 AccountOwnedByWrongProgram; missing active array →
3005 AccountNotEnoughKeys; wrong bitmap-extension account → 3007; impossible
min-out → 6003 ExceededAmountSlippageTolerance (swap2.rs:262).

**Order-tolerance finding.** The DLMM program searches the remaining accounts for
the array holding the active bin, so a REORDERED (reversed) set of valid arrays
still succeeds — the monotonic order seen in the fixtures is a caller convention,
not a program requirement. (The slice-4 reconstruction guard still enforces
monotonicity as a fixture-provenance check; it is not a chain constraint.)

Verdict: `METEORA DIRECT PARITY PROVEN — ROUTE 1 WSOL→TOKEN DIRECTION`
(Stage-1, Meteora-only). No Pump, no atomic composition, no signing/submit —
those remain out of scope.

### Direction scope (accepted)

- **Meteora WSOL→token direct parity is PROVEN.** This is the exact Meteora leg
  the current arbitrage route requires: `Meteora WSOL→Token → Pump Token→WSOL`.
  The buy direction is not a strategic deviation — it is the leg under test.
- **The opposite token→WSOL direction was represented in the CPI fixtures**
  (the captured swap2 sells) **but was NOT directly simulated.**
- **No claim is made about direct-simulation parity in the token→WSOL
  direction**, because that direction is not currently required by the strategy.
  If a future route needs it, it must get its own direct simulation before any
  such claim.
