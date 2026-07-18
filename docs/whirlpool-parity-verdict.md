# S14B-1 — Orca Whirlpool real-swap quote parity verdict

Read-only forensic slice. NO executor / signing / submission / two-leg
composition / cross-DEX observation was done. Standard: the ONLY accepted proof
is `local swap_exact_in output == observed on-chain vault delta` on a
freshness-matched snapshot. Rust-vs-TS equality is NOT evidence.

## Verdicts (per direction / variant)

| direction | swapV2 (current) | swap-v1 via CPI | swap-v1 direct |
|---|---|---|---|
| **Token→WSOL** (strategic) | **DIRECT PARITY PROVEN** | **CPI PARITY PROVEN** | QUOTE MISMATCH (sub-bps) |
| WSOL→Token | **DIRECT PARITY PROVEN** | **CPI PARITY PROVEN** | QUOTE MISMATCH (sub-bps) |

Tick-crossing (any variant): **NOT on-chain-proven** (see §Crossings).

## Pools / directions tested

Discovered on-chain (owner `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`),
deepest per market:
- WSOL/USDC `Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE` (tick_spacing 4, 400 ppm)
- WSOL/USDT `FwewVm8u6tFPGewAyHmWAqad9hmF7mvqxK4mJ7iNqqGC` (tick_spacing 2, 200 ppm)

Both classic-SPL markets (no Token-2022 / transfer fee / hook). Directions
observed: USDC→WSOL, USDT→WSOL, WSOL→USDC, WSOL→USDT.

## Capture + matching method

`whirlpool-parity-capture` (read-only): a bounded snapshot ring (pool + both
vaults + 5 tick arrays around the active tick, one `getMultipleAccounts`). A
**vault-balance change** between consecutive ring entries triggers a signature
scan of that slot window (after a ~25 s index-catch-up). A tx is matched only
when a ring snapshot's `vault_a`/`vault_b` **equal the tx pre-balances exactly**;
freshness recorded as EXACT_SLOT / PRE_SLOT_MATCH (all matches this run were
PRE_SLOT_MATCH, slot distance 1–5). Exactly one Whirlpool ix on the pool with
clean opposite-sign vault deltas is required; anything else is skipped, and
snapshots whose matching set disagrees on quote-state are rejected as AMBIGUOUS.
`amount_in` and `observed_out` come from the vault deltas — never native SOL.

## Results (57 real swaps captured, both directions, both pools)

| set | count | result |
|---|---|---|
| swapV2 (CPI + direct) | 29 | **29/29 EXACT (0 abs, 0 bps)** |
| swap-v1 via CPI | 15 | **15/15 EXACT** |
| swap-v1 direct | 13 | 2 exact, **11 QUOTE MISMATCH** |

- **Proven set (`real_swaps.json`, 46 fixtures)**: every one replays to the
  **exact** observed vault delta (0 tolerance). 32 are Token→WSOL. Both pools,
  45 distinct input sizes, both variants.
- **Discrepancy set (`discrepancies.json`, 11 fixtures)**: all **legacy `swap`
  v1 invoked directly** (never swapV2, never CPI). |diff| ≤ 11 units on outputs
  up to 1.3e11 — **< 1 bps**, local slightly high.

Exact local-vs-observed examples (proven set):
`USDT→WSOL in=2 987 698 → 39 802 623` (0 diff);
`USDC→WSOL in=69 725 843 → 929 073 780` (0 diff);
`WSOL→USDC in=10 549 478 → 791 091` (0 diff).

## Classification of the 11 differences (S14B-1 step 8)

NOT a math/parser/tick error: the identical engine (`tick_math::swap_exact_in`)
is byte-exact on all 29 swapV2 and all 15 CPI swap-v1 across the same pools and
larger sizes. A layout/fee/tick bug would hit those equally; it does not. Slot
distance does not separate exact from mismatch (swapV2-CPI mean 1.9 exact;
swap-v1-direct mean 1.8 mismatch), so it is not snapshot staleness either. The
misses are confined to **legacy `swap` v1 called directly by MEV bots** — the
most likely cause is a `sqrt_price_limit` / partial-fill parameter those bots
set on the legacy instruction (not modeled by a full-consumption exact-input
quote), which the canonical `swapV2` and Jupiter-routed `swap` do not use.
Recorded, bounded (≤ 64 u, < 1 bps) and confined to swap-v1-direct by a
committed test — NOT masked with a tolerance in the proven path.

## Crossings

The proven set is entirely **single-tick**. The one real tick-crossing swap
captured fell in the discrepancy set (swap-v1 direct, diff 11). So real
on-chain **tick-crossing parity is NOT proven**. Crossing MATH is unit-tested
(`tick_math`: `crossing_one_tick_reduces_output_vs_flat`,
`crossing_multiple_ticks`, `never_overestimates_vs_flat_upper_bound`). A clean
exact-slot crossing fixture (thinner pool or larger size) is the follow-up
before relying on crossing behavior in a live route.

## Transfer-fee findings

Both pools are classic SPL (no Token-2022). `validate_mint` rejects Token-2022
mints carrying extensions (`Token2022Unsupported`); such pools were excluded at
discovery. Transfer-fee-on-input/output behavior is therefore out of scope and
UNPROVEN — a Token-2022 whirlpool would need its own slice.

## Negative controls (all fail for their typed reason)

`negative_stale_sqrt_price_breaks_parity`, `negative_wrong_direction_…`,
`negative_wrong_pool_tick_array_rejected_by_provenance`
(`TickArrayWrongPool`), `negative_bad_start_alignment_rejected`
(`TickArrayBadStart`), `negative_malformed_tick_array_rejected`
(`TickArrayUndecodable`), `negative_excessive_amount_rejects_not_overestimates`
(rejects, never extrapolates), `negative_missing_tick_arrays_…` (crossing swap
cannot price identically without its arrays). Provenance unit tests cover
vault identity/owner/mint/authority and Token-2022 rejection.

## Provenance validated before quoting

Pool owner == Whirlpool program; decoded mints == market; vault identity ==
decoded pool vaults; vault owner ∈ {Token, Token-2022}; vault mint + authority
(== whirlpool); tick-array PDA + start alignment + embedded back-pointer ==
whirlpool; oracle PDA derivation; mint owner. All typed rejects.

## Reproducibility

- Commit: see git; fixture schema v1.
- `real_swaps.json` sha256 `4abe8e572b7afc9f40e7e30d6ff03ae2a563fc41bbf7802a45b790c62727281c`
- `discrepancies.json` sha256 `48a78c60bb0c0560e4a1481c578533a86b746b32e920d0c5ea4a72b0dd6de0f7`
- `pools.json` sha256 `928a3f34bece50d578e20872be2e216373551f7b24c8462326a648afdad29025`
- Commands: `whirlpool-parity-capture --discover` | `--capture <secs>` |
  `--verify` | `--split`. Tests: `cargo test -p arb-monitor whirlpool`.

## Is Token→WSOL proven strongly enough to justify a cross-DEX discovery slice?

**Yes, for the canonical `swapV2` instruction** (and swap-v1 via CPI): 32
Token→WSOL real swaps reproduce the exact on-chain output, both pools, wide
size range, zero tolerance. A future executor would use `swapV2`, which is
fully proven here in both directions. Combined with the already-PROVEN Meteora
DLMM leg, the **WSOL→Token (Meteora) → Token→WSOL (Whirlpool)** cycle now has
both single-tick legs quote-exact.

Two caveats to carry into that slice: (1) on-chain **tick-crossing** parity is
unit-tested only — capture a clean crossing fixture before sizing trades that
cross ticks; (2) the legacy direct `swap`-v1 sub-bps discrepancy should be
root-caused (or simply avoided by using `swapV2`) if v1 is ever constructed.
Neither blocks an observe-only cross-DEX economic-discovery slice, which is the
recommended next step (subject to the unchanged ~0.1 SOL/day economic gate).
