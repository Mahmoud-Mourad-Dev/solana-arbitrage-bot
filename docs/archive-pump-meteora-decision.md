# ARCHIVE DECISION — Pump ↔ Meteora execution strategy (S14A)

**Final status: `ARCHIVED — NOT APPROVED FOR EXECUTION`**
**Decision date: 2026-07-18** · Director decision, recorded verbatim below.
**This is an ECONOMIC rejection, not a technical failure.**

## Strategy tested

Atomic two-pool arbitrage, single direction:
**Meteora DLMM WSOL→Token (buy) → Pump AMM Token→WSOL (sell)**, WSOL-denominated,
on dynamically discovered Pump∩Meteora token markets. The opposite direction
(pump-first) is a creator-pool BUY and was refused throughout (unresolved
creator-BUY rounding; never shipped).

## Commits used (S13C chain)

| commit | content |
|---|---|
| `6a8503d` | safety foundation (SafetyGate, no-send/no-sign proofs) |
| `186acbe` | slice 1 — Pump fee-v2 account evidence persisted + validated |
| `bd26040` | slice 2 — isolated SimClient + fixture capture |
| `6bdf10f` | slice 3 — byte-exact Pump sell reconstruction ([9,10,22,23] rotate) |
| `8256a62` | slice 4 — Meteora swap2 byte-exact reconstruction |
| `6954515` | slice 5 — **METEORA DIRECT PARITY PROVEN** (0 bps, WSOL→token) |
| `d0b7100` | slice 6 — Pump sell sim parity → QUOTE MISMATCH (root cause found) |
| `fb402e7` | slice 6B — **Pump fee-v2 decoded; PUMP FEE-V2 PARITY PROVEN** (0 bps) |
| `fd843ed` | slice 6C — dynamic fee-v2 wired through the whole quote path |
| `9ae2d03` | slice 6D — fee-correct wide smoke + curated narrow set |
| `59e6fe4` | S13C correctness repair (Codex findings, P1–P9) |
| `93a9cff` | smoke follow-ups — **final run commit** |

## Simulation-parity evidence (technically PROVEN)

- **Meteora DLMM leg**: direct top-level `swap2` invocation proven viable (sole
  IDL signer = user, substitutable); local Rust quote == simulated destination
  token delta **exactly (0 abs, 0 bps)**; negative controls fail for their own
  program reasons (3005/3007/6003); same-state guard held. Proven for
  **WSOL→token only** — the strategy's required direction. Token→WSOL was
  present in CPI fixtures but never directly simulated (no claim made).
- **Pump AMM sell leg**: byte-exact reconstruction + user substitution proven;
  **dynamic fee-v2 discovered and decoded** (see below); with the decoded fee,
  local WSOL net == simulated WSOL delta **exactly (0 abs, 0 bps)** on Route 1
  (75 bps) and Route 3 (90/95 bps across a live tier change).

## Dynamic Pump fee-v2 discovery (the decisive finding)

The Pump sell fee is NOT the legacy 30 bps. It is a **market-cap-tiered dynamic
schedule** in the fee-program global config `5PHirr8j…` (owner `pfeeUxB6…`,
schema `pump-feev2-mcap24-v1`): 24 tiers at offset 109, stride 40,
`lp=20 / protocol=5 / creator=95→5` bps, tier keyed by
`market_cap = supply·quote_reserve/base_reserve`. Small/mid-cap pump tokens —
exactly this strategy's universe — pay **75–120 bps**, not 30. All earlier
observe economics (0.1127 / 0.095 SOL/day) were computed under the stale 30 bps
assumption and were therefore overstated by ~45–90 bps of the full Pump-leg
notional. Those figures are void.

## Corrected observe run (the deciding evidence)

Run at commit `93a9cff`, corrected fee-v2 pipeline + repaired measurement
(unsafe-route gate, explicit failure events, strict reconfirm survival,
provenance validation, manifest-driven offline rebuild):

| metric | value |
|---|---|
| runtime | ≈ 9.9 hours |
| poll events | 85,882 |
| safe routes | 10 |
| RPC failures | 0 |
| competitive-positive episodes | **1** (single-poll flicker) |
| survival +2s / +10s / +30s | **0 / 0 / 0** |
| independently active routes | **0** |
| route classes | 9 FrozenSpread, 1 Flicker |
| causal value at detection | ≈ **0.0659 SOL/day** |
| causal value at +2s/+10s/+30s | **0 SOL/day** |

## Exact reason the economic gate failed

The pre-agreed gate required: ≥10 competitive-positive episodes/day AND
meaningful +10s survival AND multiple active routes AND ≈0.1 SOL/day causal
competitive value. The run delivered: <1 episode per 9.9h (single-poll,
no evidence it was practically capturable), zero survival at every delay,
zero independently active routes, and 0.066 SOL/day at detection with 0 at any
realistic actuation delay. **Every prong of the gate failed.**

## Decision scope (frozen execution boundary)

- NO atomic Pump+Meteora composition.
- NO executor extension for this route; NO Jito submission; NO deployment.
- NO signing, keypair loading, or live mode; NO S10/S11 work.
- No further implementation time on executing this specific strategy.

## Three-way honesty split

**Technically proven components** (retain, reuse — see
`docs/reusable-components.md`): Pump pool decoder + exact sell quote, Pump
fee-v2 decoder, Pump sell reconstruction/substitution, Meteora DLMM
parser/quote (6/6 live-exact) + swap2 reconstruction + direct-call simulation,
snapshot provenance validation, optimizer with boundary probes, cost model,
narrow observe pipeline (events/episodes/manifest/rebuild), SafetyGate.

**Economically rejected strategy**: the Meteora→Pump WSOL round trip on
pump-token markets. Fee-v2 tiers (75–120 bps on the sell leg) consume the thin
cross-venue edge; observed spreads are dominated by frozen quotes.

**Unproven execution assumptions** (never tested; would need proof before ANY
future live work on any strategy): atomic two-leg composition, leader-timing /
inclusion probability, real Jito tip auctions, balance/rent management, signed
transaction size within limits with both legs + ALTs, live slippage vs
single-slot snapshot quotes, MEV competition response.

## Reproducibility record

- Final route config: `narrow-routes.feecorrect.json` (10 safe routes)
  sha256 `93b724e1aedefcfaf3ba57aed6a803df5752e012154e0446bc20d0bb1e17a663`.
- Final run commit: `93a9cff`; report schema v2; manifest v1; fee schema
  `pump-feev2-mcap24-v1`.
- Run artifacts (VPS): `reports/narrow-feecorrect/polls-<runid>.jsonl(.gz)` +
  `report-<runid>.json(.gz)` + `run.log`; the JSONL's first line is the run
  manifest (commit, routes, controls, timing) and `rebuild-report <jsonl>`
  reproduces the metrics with no flags.
- Evidence fixture hashes (sha256):
  - `monitor/fixtures/pump/fee_config_5PHirr8.bin`
    `e1c4647573d8caacc33b267781272c4fa0ad30a70900dddbaab512db670d3af2`
  - `monitor/fixtures/pump/fee_v2_evidence.json`
    `ae9791de2ec0e2c538dabc0f028f00a0f17efe9eaf2f9f34021a3a62a2121e78`
  - `monitor/fixtures/pump/reconstruction_fixtures.json`
    `529d7fcf3f78cd8922e4649ee1f092a42757921b406fe23830f72c03c4c66406`
  - `monitor/fixtures/meteora/swap2_cpi_fixtures.json`
    `11d1a7e32b4444d7d9b55b22b6d292120a4675a0e8075ded0d677e4311bd5d34`
- Commands: see `docs/vps-fee-correct-observe-runbook.md` (launch / status /
  safe-stop / export are unchanged and remain valid for reproduction).
- Checkpoint: annotated git tag **`archive/pump-meteora-s13c`** at `93a9cff`
  (tag only — history untouched).

Nothing was deleted: all code, fixtures, reports and documentation remain in
the tree exactly as of `93a9cff`.
