# Reusable-component inventory (S14A)

Companion to `docs/archive-pump-meteora-decision.md`. Evidence levels:
**UNIT** = deterministic unit tests only · **DIFF** = differential vs the TS
engine · **SIM** = on-chain `simulateTransaction` parity (local == simulated
token delta) · **LIVE** = matched real on-chain swap outputs / live observe.
Code existence is NOT proof of quote correctness — see limitations.

| component | module | evidence | reusable for | limitations |
|---|---|---|---|---|
| Pump AMM pool decoder | `monitor/src/pump_amm.rs` (`decode_pump_pool`) | UNIT + LIVE (parity fixtures) | any Pump-pool strategy; provenance checks | layout fixed at 301-B account observed 2026-07 |
| Pump exact SELL quote | `pump_amm.rs` (`pump_quote_detailed`, `sell_quote_with_fee_split`) | **SIM (0 bps)** + LIVE (17/17 real swaps) | any Pump sell leg | creator-pool **BUY refused** (rounding unresolved — do not lift) |
| Pump fee-v2 decoder | `monitor/src/pump_feev2.rs` | **SIM (0 bps)**, hardened schema (exact 4073 B / offset 109 / 24 tiers) | any strategy touching Pump pools; fee monitoring | supports only `pump-feev2-mcap24-v1`; fails closed on any change |
| Pump sell reconstruction + substitution | `monitor/src/pump_reconstruct.rs` | SIM (byte-exact; [9,10,22,23] rotate as one set) | future Pump execution research | fee-v2 PDAs underivable — cloning mandatory |
| Meteora DLMM parser + quote | `monitor/src/meteora_dlmm.rs` | **LIVE (6/6 exact both directions)** + UNIT | any DLMM leg (incl. Orca↔DLMM candidate) | Permission/Customizable pairs refused; Token-2022 transfer fee not modeled (screened) |
| DLMM bin-array traversal | `meteora_dlmm.rs` (bitmap walk, `InsufficientBinCoverage`) | LIVE + UNIT | any DLMM strategy | bitmap-extension arrays (|idx|>512) refused, not parsed |
| Meteora swap2 reconstruction | `monitor/src/meteora_reconstruct.rs`, `meteora_direct_call.rs` | SIM (direct-call proven, WSOL→token) | DLMM execution research | token→WSOL direction not directly simulated |
| DLMM/Pump PDA + ix builders | `monitor/src/sim_parity.rs` | SIM | simulation harnesses | sim-only by design (SafetyGate) |
| Single-slot snapshot fetch | `monitor/src/observe_live.rs` (`fetch_snapshot[_retry]`) | LIVE (85k+ events, 0 RPC failures) | any two-venue observe pipeline | Pump+DLMM shape; needs a venue-leg abstraction for new DEXes |
| Account provenance validation | `observe_live.rs` (typed rejects) | UNIT + LIVE | any venue (pattern generalizes) | checks are venue-specific; re-derive per DEX |
| Route engine (two-leg) | `monitor/src/route_engine.rs` | UNIT + SIM-backed legs | any A→B→A WSOL round trip | two legs only; leg enum must grow per venue |
| Size optimizer + boundary probes | `monitor/src/optimizer.rs` | UNIT (dominance-tested vs dense sweep) | any route family | probes assume Jito tip tiers from `common/src/cost.rs` |
| Cost model | `common/src/cost.rs` | UNIT | all strategies | tip schedule is OUR policy, not market-measured |
| Episode tracking + reconfirm survival | `monitor/src/narrow_report.rs` | UNIT (gap-split, strict survival) + LIVE | any observe campaign | competitive-net definition tied to cost model |
| JSONL run manifest + offline rebuild | `narrow_report.rs` + `bin/rebuild_report.rs` | UNIT (live==offline exact) + LIVE | any observe campaign | narrow format; wide format has no manifest yet |
| Safety-mode separation | `sim_parity.rs` (`SafetyGate`) + source-audit tests | UNIT (source-level no-send/no-sign proofs) | every future phase | env-based; executor crates remain structurally separate |
| Raydium v4 / Whirlpool legacy engine | `monitor/src/{parsers,quote,tick_math,registry,discovery}.rs` | **DIFF only** | starting point for S14B candidates | **NOT proven vs on-chain outputs**; see `docs/dex-support-audit.md` |

Retention: nothing deleted; all of the above remains at tag
`archive/pump-meteora-s13c` (= commit `93a9cff`).
