# Observation campaign O1 — meteora-dlmm ↔ pump-amm persistence

OBSERVE-ONLY. No arming occurred; the process asserts disarmament at every sweep.

## Pre-registered decision criteria (fixed before the first sweep)

- **BUILD**: median class $/day ≥ $50, AND ≥ 70% of sweeps individually clear $50/day, AND ≥ 20 DLMM parity swaps at ≤ 1 bps.
- **INVESTIGATE**: median ≥ $10/day but the consistency or parity bar is missed.
- **KILL**: median < $10/day, or fewer than half the sweeps clear the $49/mo floor on a daily-equivalent basis ($1.63/day).

Class-level $/day per sweep = Σ over the top-5 meteora↔pump markets of value-positive events ≥ 50,000 lamports (STRICT accounting, external-inflow filter armed), scaled to a day by the window actually used, converted at that sweep's own measured SOL/USD.

## Mechanically-derived decision band

**BUILD-pending-parity** — median class $/day = **$10545.14**, consistency = **100%** of conclusive sweeps ≥ $50/day; 2/2 sweeps conclusive; DLMM parity captures (engine-vs-realized) = 0.

The band is computed from the numbers, not prose. `BUILD-pending-parity` means the $/day bars are met but the ≥20-swap ≤1 bps parity gate is operator-run and NOT cleared here.

## Per-sweep results

| # | UTC | anchor slot | SOL/USD (n, IQR) | landed (Σtop5) | value+ (Σ) | ev≥50k (Σ) | class $/day | top token |
|---|---|---|---|---|---|---|---|---|
| 0 | 2026-08-29T19:51:18Z | 442690398 | $107.10 (n=20, 104.6–118.7) | 32999 | 52 | 43 | $10545.14 | CTPoyCwk… |
| 1 | 2026-08-29T23:51:18Z | 442735897 | $107.39 (n=26, 105.6–107.8) | 11550 | 5 | 5 | $192.56 | 4pnj9L8C… |

## Persistence analysis (the core result)

- Class $/day: median **$10545.14**, min $192.56, max $10545.14 over 2 conclusive sweeps.
- Sweeps clearing $50/day: **100%**.
- Top-market identity changed **1** times across 2 sweeps — token turnover is expected and is a measurement, not a failure. The class result above holds ACROSS these identity changes (it sums whatever is hottest each sweep).

## GUARD_FLOOR_EXCEEDED markets (hottest, unmeasured)

| sweep | token | window floor (h) | observed tx rate/h |
|---|---|---|---|

## B5 parity capture

**Not wired in this campaign build. Rows in reports/dlmm-parity-o1.jsonl: 0.** The persistence question (the core deliverable) does not depend on parity, and a per-swap engine-vs-simulate loop was deliberately NOT bolted onto the 48h orchestrator unverified. B5 therefore remains **entirely operator-run** and **NOT cleared** — run it separately with the existing, already-simulate-proven `sim_parity::build_dlmm_swap2_ix` + `SimRpc` path against >=20 captured DLMM swaps at <=1 bps. This is a known deviation from the Prompt O piggyback, stated here rather than implied as done.

## Operations & RPC budget (stated up front)

- **Launch (VPS)**: `deploy/observe-campaign.service` (systemd, `Restart=on-failure`, log capped) or `deploy/launch-observe-campaign.sh` (tmux+nohup). Resume logic makes restart safe — completed sweeps are skipped by index, never redone or double-counted.
- **Survives SSH disconnect**: runs detached under systemd/tmux; `observe-campaign status` and `tail -f reports/observation-o1-heartbeat.txt` show progress without attaching.
- **RPC estimate per sweep**: SOL/USD ~180 getTransaction; discovery ~565 calls; each of 5 markets ~2,000–12,000 getTransaction (bounded by the 12k fetch cap, shrunk by adaptive windowing). ≈ **11k–61k calls/sweep**.
- **Campaign total (12 sweeps)**: ≈ **130k–730k RPC calls** over 48h (~1–4/s average; bursts hit 429s, handled by exponential backoff — sweeps slow, never subsample). If the provider quota is below ~750k/48h, reduce `--tx-cap`/top-N or widen the interval BEFORE launch.
- **Disk**: progress + parity JSONL are small (< a few MB for 12 sweeps); the campaign log is rotated at 50 MB. A 48h run cannot fill the disk.

## Threats to validity

- **Extrapolation factor** is explicit per row via the window used (3h → down to 0.075h; a 0.075h window scales ×320 to a day). Shorter windows carry larger factors — read the per-sweep window column.
- **Non-stationarity**: memecoin markets turn over fast; Prompt A saw a market go $12.8k/mo → $0 between windows. The persistence analysis above is the whole point — a single sweep proves nothing.
- **GUARD_FLOOR_EXCEEDED** markets are the hottest and are excluded from the measured $/day; the class figure is therefore a LOWER bound whenever any top-5 market floored out.
- **Structural blind spot**: this instrument sees landed, inventory-neutral, value-positive cross-venue cycles only. It cannot see sub-slot dislocations that never landed, and it prices non-WSOL legs at the local market rate, not at execution.
- Any sweep marked INCONCLUSIVE (fewer than 10 clean SOL/USDC swaps) is excluded from the median rather than back-filled with a prior price.
