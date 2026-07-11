# Current Architecture (Phase 0 audit)

Snapshot of the repository **as it exists today**, before the Pump AMM ↔
Meteora DLMM pivot. Written from a full read of the code, not assumptions.

## Workspace layout (6 crates, ~7k LOC, 60 tests)

| Crate | Purpose | Reuse verdict |
|---|---|---|
| `common/` | Frozen instruction ABI (`ix.rs`) + opportunity JSON (`opportunity.rs`). `no_std`+alloc, zero Solana deps. | **Reuse** (extend ABI) |
| `monitor/` | Registry, parsers, integer quote math (CPMM + exact CLMM), cycle discovery, Geyser stream, RPC-poll `preview`, `discover-pools`, consistency guards. | **Reuse core; replace DEX-specific + discovery** |
| `program/` | Pinocchio `no_std` on-chain atomic executor. Generic hop-forwarder; on-chain profit-or-revert. | **Reuse pattern; add Pump/Meteora** |
| `executor/` | v0 tx builder, ALT, compute budget, Jito, dynamic tip, blockhash cache, `simulateTransaction`, submission gates. | **Reuse; refactor cost model + account resolver** |
| `mock-dex/` | Test-only CPI stand-in for mollusk. | Keep (test infra) |
| `bot/` | Fused monitor+executor over `tokio::mpsc`. | Keep (glue) |

## Data flow (today)

```
                     ┌─ discover-pools (Raydium/Orca HTTP APIs) → pools.generated.json
                     │
RPC bootstrap ──► PoolRegistry (per-pool state, BigInt)
   │                   ▲
   │   ┌───────────────┤ apply_account_update(pubkey, data, slot)
   │   │
   ├─ preview: RPC getMultipleAccounts poll (3s) ─┐
   └─ pipeline: Yellowstone Geyser stream ────────┤
                                                   ▼
                       DiscoveryEngine (precomputed cycle index, ≤4 hops)
                                                   │
                            quote.rs → math.rs (CPMM) / tick_math.rs (exact CLMM)
                                                   │
                            optimize_input (ternary search, 48 iters)
                                                   │
                       Opportunity (arb_common::opportunity) ──► RedisSink / mpsc / report
                                                                        │
                                                   executor::App::handle_opportunity
                                                     gates → resolver → builder → Jito/simulate
                                                                        │
                                                   program/ (CPI Raydium/Whirlpool, profit-or-revert)
```

## Supported DEXes (today)

- **Raydium AMM v4** (`675kPX9M…`) — CPMM, `x·y=k`, effective reserve = vault + openOrders − needTakePnl.
- **Orca Whirlpool** (`whirLbMiic…`) — CLMM, **exact tick-array** swap math (`tick_math.rs`), validated against live on-chain sqrt prices.
- **Nothing else.** No Pump AMM, no Meteora DLMM, no Raydium CLMM/CPMM, no order books.

## Parsers (`monitor/src/parsers.rs`)

- `decode_raydium_v4` (752 B), `decode_whirlpool` (653 B), `decode_tick_array` (9988 B), SPL token/mint, Serum OpenOrders. Fixed-offset, integer, verified against mainnet layouts.

## Quote implementations

- `math.rs`: `cpmm_amount_out` (U256), Raydium effective reserve, ternary `optimize_input`. Integer only.
- `tick_math.rs`: exact Whirlpool swap — `sqrt_price_from_tick` (U256 Uniswap chain), `compute_swap_step`, `swap_exact_in` stepping initialized ticks crossing `liquidity_net`. Conservative (rounds against trade), rejects on missing coverage. **This is the reusable pattern for DLMM bin traversal.**
- `quote.rs`: dispatch → `QuoteOutcome { Ok, NotReady, WrongMint, RaydiumDrained, WhirlpoolMissingTicks, WhirlpoolBeyondCoverage }`.

## Route generation (`discovery.rs`)

- `build_cycle_index`: DFS enumerates **all** base-anchored simple cycles length 2..=`max_hops` (default up to 4), once at startup, indexed by pool.
- `run_search(registry, cfg, fresh_floor)`: for dirty pools, re-evaluate touching routes; freshness gate (P0-3) rejects stale-pool cycles.
- `evaluate_route`: ternary-optimize input, per-hop quote, cost model, min-profit-bps gate.
- **2-pool WSOL→X→WSOL is a subset** (set `max_hops=2`, `base_mints=[WSOL]`), so the route engine substrate is reusable — only the DEX quote/parse and the pair topology change.

## Optimizer

- `optimize_input`: **ternary search only**, 48 iterations, integer, tracks best probe. No coarse grid, no multi-candidate refinement.

## Cost model (SPLIT — a real defect)

- **Monitor** (`discovery.rs:329`): cost = `base_signature_fee + priority_fee + jito_tip` (all fixed config).
- **Executor** (`tip.rs` + `config.rs:fee_lamports`): **dynamic** tip = `clamp(min, gross·tier%, max)`, tiered 50–80%.
- The monitor's fixed-tip net-profit gate and the executor's dynamic tip **disagree** → a candidate the monitor calls profitable can be unprofitable at the executor's real tip, and vice-versa. There is no single shared cost model.

## On-chain program (`program/src/lib.rs`, Pinocchio no_std, 24 KB)

- Instruction ABI (frozen in `common/ix.rs`): header (num_hops u8, amount_in u64, min_profit u64) + per-hop (dex u8, num_accounts u8, source_index u8, flags u8, min_amount_out u64). No Borsh, little-endian.
- `DexKind { RaydiumV4=0, OrcaWhirlpool=1 }`; `build_raydium_swap_data` (tag 9), `build_whirlpool_swap_data` (Anchor `swap` disc).
- Execution: record base balance → for each hop, verify `dex_program.key == expected && executable`, CPI-forward the swap, sweep full source on later hops → require `final ≥ start + min_profit` else **revert**. Enforces profitability **on-chain**. Account validation: authority signer, base account owner == authority, per-hop program-id match. **Generic forwarder** — adding a DEX = new `DexKind` + swap-data builder + program-id constant.

## Executor (`executor/`)

- `builder.rs`: v0 message, ALT, `ComputeBudget` (limit+price), arb instruction, Jito tip transfer inside the tx (reverts with it), 1232-byte size check.
- `resolver.rs`: resolves each hop into full CPI account list — Raydium (OpenBook market keys + vault signer PDA, cached) and Whirlpool (vaults + direction tick-array PDAs + oracle, TTL cache).
- `jito.rs`: `sendBundle` base64 + status probe, tip accounts.
- `app.rs`: `handle_opportunity` — staleness/economics gates → resolve → build → **simulate when not armed** / submit when `DRY_RUN=false && ENABLE_SUBMIT=true && ENABLE_JITO=true`.
- `blockhash.rs`: background-refreshed blockhash cache.

## State ingestion

- **Preview**: RPC `getMultipleAccounts` poll (3s), per-account chunk slot, spread/freshness/sleep guards (P0), single-slot confirmation gate (P1).
- **Pipeline**: Yellowstone Geyser stream (`yellowstone-grpc-client` 13), auto-reconnect+dedup.
- Account updates carry **slot** (per chunk in preview; per update in Geyser). **No** write-version, owner, lamports, source, or receive/decode timestamps recorded. No WebSocket account subscribe. No replay.

## Modes (today)

- No explicit `observe/replay/simulate/live`. Behavior is controlled by 3 flags: `DRY_RUN` (default true), `ENABLE_SUBMIT` (false), `ENABLE_JITO` (false). `preview` is effectively an observe-only binary that never submits. There is no replay harness and no simulation-parity harness.

## Jito & RPC paths

- Jito: `executor/jito.rs` (mainnet block-engine, base64 bundles, retries). Only reachable via the executor (Redis/channel), not the preview.
- RPC: `solana-client` non-blocking; preview polls; bootstrap hydrates; blockhash cache.

## Tests (60, all green)

- Integer quote math: CPMM bit-exact vs TS, Whirlpool exact vs live on-chain sqrt, tick crossings, no-overestimate, rejections.
- On-chain program: 7 mollusk tests (profit-or-revert, wrong-dex, owner mismatch, malformed).
- Consistency (P0): slot-spread, freshness gate, sleep-gap, tick-refresh-no-freeze. Confirmation (P1): `evaluate_by_id`.
- ABI freeze, TS↔Rust parity.
- **Gaps:** no Pump/Meteora anything; no replay; no simulation-parity; no grid optimizer; no shared cost-model test.

## Evidence from live runs

- Two multi-hour QuickNode dry-runs on 86 Raydium/Orca pools: **0 confirmed opportunities**. The tooling is trustworthy; the *market graph* (efficient major/mid pairs, 2-DEX cyclic) has no catchable edge. This is the reason for the pivot.
