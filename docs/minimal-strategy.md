# Minimal strategy (Prompt T, Part 2)

Scope reduction to the smallest thing that could make money. Authorised by the
Part 1 gate (`docs/tip-aware-economics.md`: PROCEED).

Safety is unchanged and was re-verified: `Mode::Observe` default, no marker
file, no submit flag, the arming assertion still aborts loudly, and **B5 is not
cleared** — the O1 "parity capture" was never wired and nothing here changes
that. The engine-quote-vs-realized capture is not engine-vs-`simulateTransaction`
parity and stays labelled as such.

## What survives (the minimal path)

| piece | where | note |
|---|---|---|
| one venue pair | meteora-dlmm ↔ pump-amm | the only pair Prompt A/O measured as live |
| one route shape | 2-hop atomic cycle, WSOL→token→WSOL | expressed by the existing `Route`/ABI |
| one submission path | **Jito bundle, always** | raw arb submission is now impossible (below) |
| one on-chain program | `program/` (Pinocchio), **unchanged** | not touched |
| core monitor pipeline | `arb-monitor` bin (`monitor/src/main.rs`) | Geyser→discovery→Redis |
| executor | `executor/` | consumes opportunities, bundles via Jito |
| shared cost model | `common/src/cost.rs` | tip-aware, single source of truth |
| forensics instrument | `monitor/src/forensics/` (library) | **stays reachable** — always compiled |

## Submission safety — raw arb submission is now impossible

The arbitrage was already Jito-only (`app.rs` calls `jito.send_bundle` and has
no raw path for the arb). Part 2 hardened the one remaining raw send:

- **ATA creation** (`ensure_ata`) was the only raw `send_and_confirm_transaction`
  in the executor, and it ran **before** the submission gate — so Observe mode
  could send a real ATA-creation transaction. **Fixed**: `ensure_ata` now gates
  on the full arming condition (`submission_armed()`), the SINGLE definition
  shared with the bundle gate. In Observe/Simulate/dry-run it records intent and
  **sends nothing**. If the raw path is ever reached while disarmed it **aborts
  hard** (`assert!`), not a config default. So a disarmed process sends zero
  transactions of any kind — this closes a real Observe-safety hole, not just a
  cosmetic one.
- `submission_armed()` = `mode.allows_live_submission()` (Live only) AND
  `!dry_run` AND `ENABLE_SUBMIT` AND `ENABLE_JITO` AND `strategy_armed()`.

## What is quarantined (kept, not deleted) — and how to restore

All auxiliary tooling is gated behind the `legacy-tooling` cargo feature
(`monitor/Cargo.toml`, **off by default**). The default build produces only the
surviving minimal path; the tooling is one flag away.

Quarantined binaries (20), each with `required-features = ["legacy-tooling"]`:
- **multi-pair discovery**: `discover-venue-pairs`, `discover-pools`,
  `discover-markets`
- **batch forensics**: `forensics-batch`, `forensics-s15b`, `forensic-arb-scan`,
  `forensic-route-recon`
- **campaign orchestrator**: `observe-campaign`
- **observe/venue tooling**: `observe-xdex`, `observe-markets`, `observe-narrow`
- **parity / capture / sim**: `parity_harness`, `dlmm_quote_cli`,
  `capture-parity-fixtures`, `whirlpool-parity-capture`, `sim-meteora-route1`,
  `sim-pump-sell`, `reprice-pump-fees`, `rebuild-report`, `preview`

**Restore any of them:**
```bash
cargo build --release -p arb-monitor --features legacy-tooling --bin forensics-batch
# or all tooling at once:
cargo build --release -p arb-monitor --features legacy-tooling
```

The **forensics instrument stays reachable** as required: its library
(`monitor/src/forensics/`) is always compiled and unit-tested in the default
build; only its *binaries* need the flag. Any future "is this pair still alive"
question is one `--features legacy-tooling` rebuild away.

## Whirlpool / Raydium venue support — recommended, not executed (with rationale)

The prompt lists whirlpool/raydium venue support for quarantine. I did **not**
feature-gate it out of the executor/program this session, deliberately:

1. **The program must stay unchanged** (an explicit survivor). Its `DexKind`
   branches and the executor `resolver` share one code path with the mollusk
   tests, which use a Raydium-id mock. Gating them risks the "program unchanged"
   and "tests green" constraints for no safety gain.
2. **The forensics instrument (a survivor) needs all five venue adapters**
   (`forensics/venues.rs`) to answer future liveness questions. Removing venue
   decoders would break the reachable instrument.
3. No safety gate lives in the venue code, so leaving it in place removes no
   protection.

**Recommendation** (a separate, test-guarded change): once pump-amm execution
exists, feature-gate the `raydium_hop`/`whirlpool_hop` resolver methods and their
`DexKind` branches behind a `legacy-venues` feature, moving the mollusk
Raydium-mock test under the same flag. Left as a recommendation because it is
invasive and not safety-critical.

## Known gap: pump-amm execution is not implemented

The surviving pair is meteora-dlmm ↔ pump-amm, but `DexKind` has no `PumpAmm`
variant and neither the program nor the resolver can execute a pump-amm hop
(only Meteora execution was added in Prompt B). **The minimal strategy is not
yet end-to-end executable** — pump-amm CPI (program) + resolution (executor) is
the next build step. Scope reduction does not create this; it only narrows what
exists. Stated plainly so it is not mistaken for a working pipeline.

## Repo hygiene

Working tree **7.1 GB**, dominated by build artifacts:

| path | size | committed? |
|---|---|---|
| `target/` | 7.0 GB | **gitignored** ✓ |
| `node_modules/` | 77 MB | **gitignored** ✓ |
| `reports/` (incl. `*.jsonl`) | 936 KB | **gitignored** ✓ (progress, parity, tip samples all ignored) |
| `.git/` | 1.6 MB | — |
| `monitor/fixtures/` | 1.8 MB | committed (largest: `whirlpool/real_swaps.json` 1.2 MB) |

- `target/`, `node_modules/`, and the large `reports/*.jsonl` are all gitignored
  — confirmed via `git check-ignore`.
- No oversized artifact is committed; the largest tracked file is a 1.2 MB
  fixture. **No git-history rewrite is needed or recommended.**
- **Recommendation (not executed)**: `cargo clean` reclaims ~7 GB instantly
  (regenerable). The earlier ~30 GB was almost certainly `target/` bloat from
  repeated release+SBF builds; a periodic `cargo clean` keeps it bounded. Not run
  here because it forces a full rebuild.

## Run the single surviving pipeline in Observe mode

Observe is the default; no arming vars, no marker. Monitor (discovery→Redis):
```bash
cargo run --release -p arb-monitor --bin arb-monitor
```
Executor (consumes opportunities, simulates only — sends nothing while disarmed):
```bash
# MODE unset ⇒ Observe; ENABLE_SUBMIT/ENABLE_JITO absent ⇒ disarmed.
cargo run --release -p arb-executor
```
The executor will resolve and `simulateTransaction` each opportunity and log
`SIMULATION ONLY (submission disarmed)`; `ensure_ata` logs intent without
sending. Nothing reaches the chain until every arming precondition is set
deliberately.

## Acceptance gate

`cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets
--features arb-monitor/legacy-tooling -- -D warnings` clean; `cargo test
--workspace --features arb-monitor/legacy-tooling` **304 passed / 0 failed**;
mollusk integration suite green; executor builds; default (minimal) build
produces only `arb-monitor`.
