# solana-arbitrage-bot

Cyclic arbitrage system for Solana mainnet-beta, in three layers:

| Layer | Tech | Path |
|---|---|---|
| Shared wire/ABI types | Rust, `no_std` + alloc, zero Solana deps | `common/` |
| Price monitor + discovery | TypeScript (production) — Rust port validated | `src/` → `monitor/` |
| On-chain atomic executor | Rust **Pinocchio** (`no_std`, no Anchor, no solana-program) | `program/` |
| Off-chain execution bot | Rust, tokio, Jito Block Engine (base64 bundles) | `executor/` |
| **Fused single-process bot** | Rust — monitor + executor via `tokio::mpsc` | `bot/` |

### Fused bot (Phase D)

`arb-bot` runs the monitor pipeline and the executor in **one process**,
connected by an in-process bounded `tokio::sync::mpsc` channel — the
`DiscoveryEngine` feeds opportunities straight to the executor with no
serialization and **no Redis on the hot path**. The channel uses `try_send`,
so a slow executor drops stale opportunities rather than stalling Geyser
ingestion. Redis becomes optional: `MONITOR_REDIS_MIRROR=true` also publishes
for external observability, off the critical path.

The monitor pipeline (`arb_monitor::pipeline`) and executor core
(`arb_executor::app::App`) are shared library code, so the standalone
`arb-monitor` + `arb-executor` (Redis-decoupled) and the fused `arb-bot` run
the exact same logic. Safety posture is unchanged — still simulates unless
`DRY_RUN=false` AND `ENABLE_SUBMIT=true` AND `ENABLE_JITO=true`.

```bash
cargo run -p arb-bot            # everything in one process (reads the same .env)
# or the decoupled pair:
cargo run -p arb-monitor        # Geyser -> discovery -> Redis
cargo run -p arb-executor       # Redis -> Jito
```

### On-chain program — Pinocchio migration (Phase E)

`program/` is a `no_std` Pinocchio program. The instruction ABI (17-byte
header, 12-byte hops, custom error codes) is **frozen in `common/` and
unchanged** from the previous `solana-program` build — the executor needed
zero wire changes. A mollusk acceptance-contract suite
(`program/tests/integration.rs`, 7 tests + a `mock-dex` CPI stand-in) runs
unchanged against both builds, asserting exact revert codes and a real
CPI + profit-check success path. Measured before → after:

| | solana-program | Pinocchio | Δ |
|---|---|---|---|
| Binary size | 33,400 B | 24,296 B | **−27%** |
| Success-path CU | 7,243 | 5,461 | **−25%** |

```bash
cd program && cargo build-sbf          # -> target/deploy/arbitrage_program.so
cargo test -p arbitrage-program        # acceptance contract (needs the two .so in tests/fixtures)
```

**Rust-only migration status:** Phases A + B + C(partial) + E done.
- `common/` — frozen instruction ABI + opportunity JSON types (program
  parses, executor encodes, monitor produces; one definition, no drift).
- `monitor/` — full Rust price monitor: Yellowstone Geyser gRPC (client
  13.x, library-managed reconnect/dedup) → binary parsers → pool registry
  → precomputed cycle index + discovery → Redis publisher emitting the
  **same JSON** as the TypeScript monitor. Quote math runs on U256/U512
  (not u128) because the Whirlpool `L<<64 · S0` term reaches 2^320.
  Bit-exact parity with the compiled TS math is asserted by tests using
  reference vectors captured from `dist/`.

The TypeScript monitor in `src/` stays the production producer until
`monitor/` is validated against live traffic side-by-side. Submission is
disarmed by default: real bundles require `DRY_RUN=false` **and**
`ENABLE_SUBMIT=true` **and** `ENABLE_JITO=true`.

```bash
cargo run -p arb-monitor    # Rust monitor (reads the same .env + pools.json)
```

### Differential parity (offline)

`validation/` proves the Rust engine emits the **same opportunities** as the
TypeScript engine. One fixture file (`scenarios.json`) is fed to both
`DiscoveryEngine`s; a semantic diff asserts every id, amount, per-hop leg,
net profit, bps and slot is identical (key order / int-vs-float ignored).
Covers a 2-hop arb, a 3-hop cycle, and gating cases (drained pool,
waiting-trade status). Fully offline — no Geyser/Redis needed.

```bash
npm run verify:parity        # deterministic, offline
```

### Build a richer pool graph — `discover-pools`

The 4 default pools form only 6 cycles. `discover-pools` fetches live
Raydium AMM v4 + Orca Whirlpool listings, filters by liquidity/volume, and
keeps only pools that can actually sit on a cycle anchored at a base mint
(token appears in ≥2 pools, base-connected component; isolated/dead/drained
pools dropped). Writes `pools.generated.json` (never touches `pools.json`).

```bash
DISCOVER_MAX_POOLS=100 cargo run -p arb-monitor --bin discover-pools
POOLS_FILE=pools.generated.json cargo run -p arb-monitor --bin preview
```

Observed scaling (base = SOL/USDC, ≤3 hops): 25→74 routes, 50→224,
100(→84 after prune)→674 routes. Env: `DISCOVER_MIN_TVL_USD`,
`DISCOVER_MAX_TVL_USD`, `DISCOVER_MIN_VOL24H_USD`, `DISCOVER_MAX_POOLS`,
`DISCOVER_MAX_HOPS`, `POOLS_OUT`.

**Whirlpool quotes are exact (tick-array aware).** `tick_math.rs` steps the
swap through initialized ticks, crossing `liquidity_net` at each boundary,
exactly as the Whirlpool program does — conservative (inputs round up,
outputs down; rejects rather than estimates when tick data is missing or the
swap would exceed loaded coverage). This removed the single-tick
phantom-profit class: the previously persistent ~3% WBTC/JLP triangle no
longer appears. The `sqrt_price_from_tick` constants are validated against
live on-chain `(tick, sqrtPrice)` pairs, and a regression test asserts a
thin-pool phantom that the old approximation "profited" now yields no
opportunity. Bootstrap fetches ±2 tick arrays per whirlpool per direction and
logs any pool with no usable tick liquidity (its quotes are rejected).

> Note: with exact quoting the preview correctly shows **no** persistent
> edges on efficient pools at poll latency — real cyclic arb is sub-second.
> A surfaced candidate is now exact-math-validated, but still needs on-chain
> `simulateTransaction` + low-latency (Geyser) execution to actually capture.

### Dry-run preview — no Geyser, no money

Before paying for a Geyser subscription, find out whether your `pools.json`
even produces profitable cycles. `arb-preview` runs the **exact same
registry + discovery engine** as production, but sources pool state by
polling plain **public RPC** instead of a Geyser stream — free, no keypair,
no Redis, nothing ever submitted.

```bash
RPC_ENDPOINT=https://api.mainnet-beta.solana.com \
  cargo run -p arb-monitor --bin preview
```

It hydrates every pool from chain, builds the cycle index, then every few
seconds re-polls and re-runs discovery, logging any profitable cycle with
its route and economics.

#### Private RPC (required for multi-hour runs)

A rich pool set watches hundreds of accounts per poll; the public
`api.mainnet-beta.solana.com` endpoint will rate-limit that across hours.
Use a free-tier private RPC — any of these work, no payment needed:

| Provider | Get a URL | Free tier |
|---|---|---|
| Helius | dashboard.helius.dev → create API key | ~generous; `https://mainnet.helius-rpc.com/?api-key=<KEY>` |
| QuickNode | quicknode.com → create Solana mainnet endpoint | free plan; copyable HTTPS URL |
| Triton (RPC Pool) | triton.one | free tier; `https://<you>.rpcpool.com/<token>` |
| Alchemy | alchemy.com → Solana app | free plan; `https://solana-mainnet.g.alchemy.com/v2/<KEY>` |

Put it in `.env` as `RPC_ENDPOINT=...` (the same key you'll later reuse for
bootstrap). These are HTTP RPC endpoints and are unrelated to the paid
**Geyser** subscription — this is exactly the point of the preview: prove the
edge exists on a free RPC before paying for Geyser streaming.

**Multi-hour runs** auto-stop and write a cumulative report:

```bash
# 6 hours on a PRIVATE RPC, then report
RPC_ENDPOINT=<your-private-rpc> POOLS_FILE=pools.generated.json \
  PREVIEW_DURATION_SECS=21600 PREVIEW_POLL_INTERVAL_MS=3000 \
  cargo run --release -p arb-monitor --bin preview
```

The report (stdout + `preview-report.json`) covers runtime, poll
success rate, whirlpool quote health, unique cycles seen, most-persistent
cycles (a long-lived one is a quirk to investigate, not a repeatable
profit), best net profit, and hourly opportunity counts. Tick-array coverage
is refreshed every `PREVIEW_TICK_REFRESH_SECS` so exact quotes stay valid as
prices drift. Ctrl-C also finalizes the report. Use it to decide: if cycles show up here (at slow
poll latency), Geyser will catch far more; if nothing ever appears, your
pool set needs work first — add more pools that **share tokens** (more edges
= more cycle paths) and less efficient / more volatile pairs, since deep
major pairs like SOL/USDC are arbitraged flat.

Caveat: polling latency (seconds) means these are **not executable** — they
would be stale and contested. It is a feasibility probe, not a trading loop.
Quotes are conservative (single-tick CLMM, fees in), so real edge is ≥ shown.

### Live side-by-side parity

`validation/live/` runs the TypeScript and Rust monitors against the **same
live Geyser feed + pools.json**, each publishing to a distinct Redis channel.
A correlator subscribes to both and matches opportunities by `(id, slot)` —
when both engines emit the same cycle id at the same slot they observed
identical on-chain state, so every amount/profit/leg **must** match; any
divergence there is a real bug. One-sided emissions (independent stream
timing / per-engine cooldown) are reported separately. Neither monitor
submits anything — they only read chain state and publish.

```bash
npm run parity:selftest      # offline: verify the correlator logic
npm run verify:parity:live   # live: needs GEYSER_ENDPOINT + reachable Redis in .env
#   bash validation/live/run-live.sh 120   # auto-stop after 120s
```

Exit code is non-zero if any divergence was seen — CI-friendly. Per-monitor
output goes to `validation/live/{ts,rs}-monitor.log`.

```bash
npm run verify:parity   # build TS + run both engines + compare
```

Live side-by-side validation (run `npm run dev` and `cargo run -p
arb-monitor` against the same Geyser + Redis and compare the published
stream) is the remaining step and needs a real `GEYSER_ENDPOINT`.

```
Geyser ─> TS monitor ─> Redis PUBLISH arbitrage_opportunities
                              │
                              v
                        arb-executor ─> Jito bundle
                              │            (CU budget + program ix + tip)
                              v
                     arbitrage-program ─> CPI swaps ─> profit check or revert
```

## Layer 1 — Price Monitor (TypeScript)

Streams Raydium AMM v4 and Orca Whirlpool state over Yellowstone Geyser
gRPC, maintains an exact BigInt in-memory book, and broadcasts profitable
cycles to Redis for the execution layer.

### Data flow

```
Yellowstone Geyser (processed commitment, one bidi gRPC stream)
  └─ account updates: pool accounts + SPL vaults + Raydium OpenOrders
       └─ src/parsers/*  (fixed-offset binary decoding, zero deps)
            └─ PoolRegistry (Map, BigInt state, slot-ordered writes)
                 ├─ RedisSink        pipelined HSET pool:<address> mirrors
                 └─ DiscoveryEngine  precomputed cycle index; on update,
                                     only routes touching the dirty pool
                                     are re-simulated (ternary-search
                                     input sizing, exact CPMM/CLMM math)
                      └─ profitable? -> LPUSH + PUBLISH arbitrage_opportunities
```

Key design decisions:

- **Raydium v4 reserves are not in the pool account.** Effective reserve =
  vault balance + OpenOrders totals − `needTakePnl`, so the monitor
  subscribes to all three account types per pool. Swap fee is parsed from
  the account (canonically 25/10000).
- **Whirlpool quoting** uses exact Q64.64 sqrt-price math within the
  current tick, rounding against the trade. Quotes whose implied price
  move exceeds `MAX_CLMM_IMPACT_BPS` are rejected rather than
  overestimated (tick-crossing guard).
- **No graph traversal on the hot path.** All cycles (≤ `MAX_HOPS`) are
  enumerated once at startup and indexed by pool; a Geyser packet costs a
  dirty-set insert plus O(routes touching that pool) simulations, chunked
  under a 4ms budget on `setImmediate`.
- **All balances are BigInt.** Floats appear only in log formatting.

## Setup

```bash
npm install
cp .env.example .env   # fill GEYSER_ENDPOINT / GEYSER_X_TOKEN / REDIS_URL
# edit pools.json — only pool address + dex kind needed; the rest is
# hydrated from chain at bootstrap
```

## Run

```bash
npm run build && npm start     # production
npm run dev                    # tsx, no build step
```

## Verify

```bash
npm run selftest               # offline: math, optimizer, engine end-to-end
npm run verify:layouts         # live: decode real mainnet accounts and
                               # cross-check implied prices across venues
```

## Consuming opportunities

Each message on the `arbitrage_opportunities` channel (and list) is a JSON
`ArbitrageCycle` (BigInts as decimal strings): ordered hops with pool id,
dex, input/output mint, exact `amountIn`, `expectedAmountOut` and a
slippage-floored `minAmountOut`, plus net profit after fees, priority fee
and Jito tip. See `src/types.ts` for the authoritative shape.

## Layer 2 — On-chain program (`program/`)

Generic atomic multi-hop executor. Instruction data carries the hop list
(dex tag, account count, source-account index, direction flag, per-hop
minimum out); accounts carry `[authority, base_token_account, hop slices]`
where each slice is `[dex_program, ...CPI accounts in DEX order]`.

Execution: record starting balance of the base token account → CPI every
hop (hop 0 uses `amount_in`, later hops sweep the full output of the
previous leg) → require `final >= start + min_profit` or revert with
`ProfitNotMet` (custom error 8). Because the Jito tip transfer sits in the
SAME transaction, a reverted cycle also reverts the tip — a failed attempt
costs only the transaction fee. Zero inventory risk.

Safety checks: authority must sign; the base account's owner field must be
the authority; each hop's program id must equal the declared DEX (no
program substitution); account slices must consume the account list
exactly. No Borsh, fixed-offset parsing, one balance read per hop — CU
stays minimal (the `.so` is 33KB).

```bash
cargo build-sbf                                    # in program/
solana program deploy target/deploy/arbitrage_program.so
# put the printed program id into .env as ARB_PROGRAM_ID
```

## Layer 3 — Execution bot (`executor/`)

- Subscribes to `arbitrage_opportunities` (auto-reconnect with backoff);
  each message is handled on a bounded task pool so a slow RPC can never
  dam the stream.
- Gates: staleness (`MAX_OPPORTUNITY_AGE_MS`), per-cycle resubmit
  cooldown, and economics — dynamic tip
  `clamp(MIN_TIP, gross_profit × tier%, MAX_TIP)` with the profit share
  scaling 50%→80% as opportunities get fatter, then requires
  `gross > tip + fees + PROFIT_MARGIN_LAMPORTS` or skips.
- Resolves each hop's full CPI account list live: Raydium pool → OpenBook
  market accounts + vault-signer PDA (cached forever); Whirlpool → vaults,
  direction-correct tick-array PDAs and oracle (TTL cache). Missing ATAs
  are created idempotently on first sight.
- Builds one v0 transaction — compute budget, program instruction, tip to
  a random official Jito account — signs it against a
  background-refreshed blockhash, enforces the 1232-byte packet limit,
  and POSTs it as a bundle with bounded retries.
- `min_profit` sent on-chain = tip + fees + margin, so the program's
  balance check guarantees end-to-end profitability, not just gross gain.

```bash
cargo run -p arb-executor --bin create-alt pools.json  # once: build the ALT
DRY_RUN=true cargo run -p arb-executor                 # simulate only
cargo run -p arb-executor --release                    # live
```

## Verify (Rust layers)

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace   # 15 tests incl. cross-crate encode/parse identity
```
