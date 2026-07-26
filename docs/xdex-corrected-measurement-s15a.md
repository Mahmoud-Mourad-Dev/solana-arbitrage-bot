# S15A — Corrected selection, direct measurement, and what it proves

READ-ONLY. No transaction building, signing, submission, or executor work.
Follows `docs/forensic-route-recon-s15a.md`, which showed that S14B-3 archived a
strategy that is demonstrably profitable on-chain.

## Three selection errors — all found, all fixed

| # | error | fix | commit evidence |
|---|---|---|---|
| 1 | `mint_safety` rejected **USDC** (`HasMintAuthority` — Circle can mint), excluding every USDC route from the wide scan | `screen_mint_for_trading` + explicit two-entry `MAJOR_ASSETS` allowlist; structural checks never waived | 6 new tests |
| 2 | `observe-xdex` kept only the **top-2 pools by depth** per venue, so only one fee tier per pair was ever considered | `XDEX_POOLS_PER_VENUE` (default 6) enumerates all fee tiers / tick spacings | route count 60→64 with USDC present |
| 3 | ~1,089 safe tokens iterated in arbitrary order, hitting `max_routes` before reaching USDC | major assets sorted **first**, then by depth | USDC routes: 0 → 64 |

**All three share one root cause: I optimised for liquidity, but arbitrage lives
in liquidity *asymmetry*.** The operators trade a **shallow** pool against a
**deep** one — `BSddxwYW` holds **3.9 SOL**, `3PyikuAr` **2.16 SOL**. A shallow
pool's price moves on a modest trade; a deep one's does not. Ranking *both*
venues by depth structurally guarantees selecting two deep pools, between which
no dislocation can exist.

## Direct measurement of the exact operator pools

To remove every remaining heuristic, `observe-xdex --pairs` was added: it builds
routes from explicit `meteora:whirlpool` addresses, bypassing discovery entirely.
Measured the six pool pairs the profitable operators actually traded:

| meteora | bin step | whirlpool | tick spacing | fee |
|---|---|---|---|---|
| `3PyikuAr` | 50 | `BSddxwYW` | 32,896 | 100 ppm |
| `FbkX1h2Y` | 16 | `BSddxwYW` | 32,896 | 100 ppm |
| `5XRqv7LC` | 50 | `BSddxwYW` | 32,896 | 100 ppm |
| `3PyikuAr` | 50 | `Esvfxt3j` | 2 | 200 ppm |
| `5XRqv7LC` | 50 | `83v8iPyZ` | 1 | 100 ppm |
| `FbkX1h2Y` | 16 | `83v8iPyZ` | 1 | 100 ppm |

All six validated: both venues share exactly {WSOL, USDC}, provenance checks
pass, correct fee tiers decoded.

**Result — 25 sweeps @ 2 s, 125 valid polls:**

| metric | value |
|---|---|
| gross-positive candidates | **0 / 125** |
| best (maximum) gross edge | **−2,073 lamports** (−0.000002 SOL) |
| median gross edge | −22,695 lamports |
| worst | −181,931 lamports |
| single-tick clamp binding | 50 / 125 polls |

## What this proves

The steady-state price relationship between these pools is **flat to within
~2,000 lamports** — the market is continuously arbitraged. Yet the same pools
produced **+2,500,000 lamports median realized profit** for three operators
across 53 hours (forensic record, `docs/forensic-route-recon-s15a.md`).

**That is a ~1,000× gap between what a 2-second poller sees and what is actually
being earned.** It is not a measurement error and not a missing fee: it is the
signature of an opportunity that exists for a **fraction of a slot** immediately
after a swap lands, and is gone before the next poll. A poller samples the
*steady state between* opportunities, which is by definition flat.

**Conclusion: polling cannot measure this class of opportunity — at any interval
short of sub-slot.** The strategy is real; the *instrument* was wrong.

## Correction to my earlier analysis

In the Phase-0 review I partly disagreed with the director's diagnosis
("polling-latency discovery… real cyclic arb on Solana is sub-slot"), arguing the
archived strategies died because measured gross edge was zero rather than because
the trigger was slow. **That was wrong in an important way.** The gross edge
measured zero *because the polling observer structurally cannot see sub-slot
dislocations*. The director's diagnosis was more correct than I credited.

What I said that remains true: the **executable** path (`monitor/src/pipeline.rs`)
is already event-driven — Geyser at *processed* commitment → `apply_account_update`
→ `mark_dirty` → synchronous `run_search` over a precomputed per-pool cycle index.
The polling was only ever in the observe/research tools. So the right instrument
already exists in this repo; it has simply never been pointed at these pools.

## Status

- Raydium CLMM: **closed** (0.011 SOL, 8 txs across both forensic scans).
- Meteora ↔ Whirlpool: **not archivable on the current evidence.** The S14B-3
  archive rested on three selection errors, now fixed; the corrected steady-state
  measurement cannot see the opportunity either way.
- **Nothing here justifies a build.** What it justifies is one more measurement,
  with the correct instrument.

## Recommended next slice (read-only, no build)

Point the **existing** event-driven engine at these six pools:

1. Subscribe via Geyser (processed commitment) to the 6 Meteora pairs + their bin
   arrays and the 3 Whirlpool pools + their tick arrays.
2. On every account update, evaluate the affected routes **synchronously in the
   update handler** — the path `pipeline.rs` already implements.
3. Record, per detection: edge size, the slot it appeared in, and how many slots
   until it decays to ≤0.

That measures the three things still unknown — **frequency, size, and decay** —
with the only instrument capable of seeing them. If dislocations appear at a
usable rate and survive more than ~1 slot, an executor becomes worth discussing.
If they appear but decay within the same slot, the honest verdict is that this
edge requires infrastructure (co-location, staked connections) we do not have,
and the programme should close on that basis rather than on a mismeasurement.

**Gate:** `cargo fmt --all --check` clean, `cargo clippy --workspace
--all-targets -D warnings` clean, `cargo test --workspace` **257 passed / 0
failed** (+5 allowlist tests).
