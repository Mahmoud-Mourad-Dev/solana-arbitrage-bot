# S14B-3 — Wider Meteora DLMM ↔ Orca Whirlpool shared-market discovery

Quote-only, read-only. Cycle: **WSOL → token on Meteora DLMM → WSOL on Orca
Whirlpool swapV2**. NO executor / tx-build / keypair / signing / two-leg
simulation / Jito / submission / deployment / S10-S11 / live / long run.
Legacy Whirlpool `swap-v1` never used; Whirlpool leg single-tick clamped.

## Method (`observe-xdex --wide`)

1. Enumerate ALL WSOL-paired pools on both venues from on-chain program
   accounts (`getProgramAccounts`, dataSize + WSOL-mint memcmp on each side).
2. Join strictly by exact non-WSOL token mint (token present on BOTH venues).
3. Screen each shared token mint (`mint_safety::screen_mint`, pure + tested):
   classic SPL only, base 82-byte layout (no Token-2022 extensions), **no mint
   authority, no freeze authority**, initialized. Typed rejects.
4. Liquidity gate: WSOL-vault balance ≥ 2 SOL on BOTH legs (batched reads).
5. Enumerate combinations — top-2 deepest Meteora × top-2 deepest Whirlpool per
   token (not only the single deepest), capped at `XDEX_MAX_ROUTES` (≤50).
6. Per route, single-slot-per-leg snapshot with full provenance (pool owner,
   decoded mints == market, vault identity/owner/mint/authority, tick-array
   PDA+start+back-pointer, Meteora sides/reserves), Whirlpool single-tick clamp.
7. Shared route engine + optimizer + `aggregate_narrow` (offline == live).

Enforced throughout: swapV2 semantics only, single-tick Whirlpool capacity,
proven Meteora bin coverage, no optimistic fallback, no legacy swap-v1.

## Enumeration result (mainnet)

| quantity | value |
|---|---|
| Meteora WSOL pools enumerated | **110,423** |
| Whirlpool WSOL pools enumerated | **7,345** |
| shared token mints (both venues) | **1,455** |
| safe token mints (strict filter) | **1,093** |
| validated route combinations built | **40** (capped; ~50 available) |

Rejections (typed): mint HasMintAuthority 181, Token-2022 173,
HasFreezeAuthority 8; whirlpool_empty 18,840; meteora_thin 140,
whirlpool_thin 122; meteora_disabled 8. Note: USDC/USDT (the S14B-2 pair) are
correctly EXCLUDED by the strict filter — both carry mint+freeze authorities.

## Wide smoke (5 sweeps, ~4 min, XDEX_MAX_SOL 5, 40 routes)

| metric | value |
|---|---|
| sweeps | 5 |
| events | 200 |
| valid polls | **125** (75 rejected: 14 Orca dynamic/variable tick arrays `TickArrayUndecodable`, rest thin/undecodable) |
| **gross-positive candidates** | **0** |
| **competitive-positive candidates** | **0** |
| best gross edge | **0 lamports** |
| best competitive net | 0 lamports |
| selected sizes | 0 (no profitable size on any route) |
| single-tick clamp binding | 20 / 125 valid polls |
| RPC failures | 0 |
| cadence | ~40 s per full 40-route sweep (250+ sequential reads) |
| offline == live metrics | **true** |
| episodes | 0 (all 40 routes `NeverProfitable`) |

Reproduced across two independent runs (1-sweep/50-route and 5-sweep/40-route):
**zero gross-positive in every case.** Not one of 40 diverse volatile shared
markets showed a positive fee-less round-trip — the two on-chain AMMs are
continuously arbitraged against each other, so the intra-slot WSOL→token→WSOL
round trip loses to the combined DLMM + Whirlpool fees before any cost model.

## Narrow config

Per spec §9, a narrow configuration is created ONLY when genuine gross-positive
candidates appear. **None appeared → no narrow config was created.**

## Decision (spec §10)

Zero gross-positive candidates across the expanded ~1,093-token universe (40
validated routes, 5 sweeps):

**`ARCHIVE METEORA ↔ WHIRLPOOL STRATEGY`**

This is an economic rejection, not a technical failure. The discovery pipeline,
mint-safety filter, single-tick clamp, shared route engine, and provenance
checks are proven and retained as reusable components for any future DEX pair.
Do NOT proceed to a 10–12 h run or atomic composition on this venue pair — a
long run cannot rescue a universe with no gross edge at the snapshot level.

Gate: fmt, clippy `--workspace -D warnings`, `cargo test --workspace` 246
passed / 0 failed.
