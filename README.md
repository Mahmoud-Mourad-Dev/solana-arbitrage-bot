# solana-arbitrage-bot

Cyclic arbitrage system for Solana mainnet-beta, in three layers:

| Layer | Tech | Path |
|---|---|---|
| Shared wire/ABI types | Rust, zero Solana deps | `common/` |
| Price monitor + discovery | TypeScript (production) — Rust port in progress | `src/` → `monitor/` |
| On-chain atomic executor | Rust (native `solana-program`, no Anchor; Pinocchio planned) | `program/` |
| Off-chain execution bot | Rust, tokio, Jito Block Engine (base64 bundles) | `executor/` |

**Rust-only migration status:** Phases A + B done.
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
