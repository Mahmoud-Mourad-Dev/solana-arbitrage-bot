# Fee-correct 24h narrow observe — VPS runbook (S13C slice 6D)

Observe-only. NEVER builds, signs, simulates, or submits. Uses the dynamic Pump
fee-v2 model wired in slice 6C (commit `fd843ed`): every Pump sell quote reads
the live fee tier from the single-slot snapshot — no 30 bps fallback.

Curated route set: `narrow-routes.feecorrect.json` (top dual-venue markets by arb
capacity + the two fee-v2-parity-proven routes). Fees are evaluated dynamically
per snapshot at run time.

The final 24h report headline uses CAUSAL value at detection and the +2s/+10s/
+30s reconfirms (one theoretical capture per episode), the dynamic fee tier from
each snapshot, frozen controls excluded — never the hindsight maximum.

## Prerequisites (on the VPS)
- Rust toolchain; repo checked out at commit `fd843ed` or later.
- `.env` with `RPC_ENDPOINT=...` (kept out of logs — the binaries redact it).
- Build once: `cargo build --release -p arb-monitor --bin observe-narrow`

## Launch (24h, detached)
```bash
cd /path/to/solana-arbitrage-bot
mkdir -p reports/narrow-feecorrect
MODE=observe \
OBS_DURATION_SECS=86400 \
NARROW_INTERVAL_SECS=3 \
NARROW_FROZEN_SECS=45 \
OBS_MAX_SOL=20 \
OBS_OUT_DIR=reports/narrow-feecorrect \
nohup ./target/release/observe-narrow --cache narrow-routes.feecorrect.json \
  > reports/narrow-feecorrect/run.log 2>&1 &
echo $! > reports/narrow-feecorrect/run.pid
```

## Status
```bash
# live tail (ANSI-stripped)
sed 's/\x1b\[[0-9;]*m//g' reports/narrow-feecorrect/run.log | tail -30
# is it alive?
kill -0 "$(cat reports/narrow-feecorrect/run.pid)" && echo RUNNING || echo STOPPED
# rough episode count so far
grep -c '"profitable_competitive":true' reports/narrow-feecorrect/polls-*.jsonl 2>/dev/null
```

## Safe stop (graceful flush — writes the partial report)
```bash
kill -INT "$(cat reports/narrow-feecorrect/run.pid)"   # SIGINT: flush + final report
# (SIGTERM also flushes; never kill -9 — that loses the report)
```

## Report export
```bash
ls -lt reports/narrow-feecorrect/report-*.json.gz | head -1
# copy back to your machine, e.g.:
#   scp vps:/path/reports/narrow-feecorrect/report-*.json.gz .
tar -czf narrow-feecorrect-$(date +%s).tar.gz reports/narrow-feecorrect/
```

## Decision gate (after the 24h run)
- Corrected CAUSAL competitive value materially **below ~0.1 SOL/day** at realistic
  actuation ⇒ recommend `ARCHIVE PUMP↔METEORA STRATEGY`.
- **≈0.1 SOL/day or higher**, with multiple active routes and meaningful +10s
  survival ⇒ reassess whether atomic simulation is economically justified.

Do not reuse the pre-6B `0.1127 / 0.095 SOL/day` figures — they used the stale
30 bps fee and are invalid for decisions.
