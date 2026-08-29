#!/usr/bin/env bash
# Launch the Prompt O persistence campaign so it survives SSH disconnection.
# Prefers systemd; falls back to tmux + nohup. Observe-only — the binary
# aborts if any arming precondition is present.
set -euo pipefail
cd "$(dirname "$0")/.."

# 1. Gate before launch (never launch red).
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
SBF_OUT_DIR="$PWD/program/tests/fixtures" cargo test --workspace
cargo build --release -p arb-monitor --bin observe-campaign --bin discover-venue-pairs
./target/release/observe-campaign preflight

# 2. Rotating log guard: cap the campaign log at ~50 MB.
mkdir -p reports
LOG=reports/observe-campaign.log
if [ -f "$LOG" ] && [ "$(wc -c <"$LOG")" -gt 52428800 ]; then
  mv "$LOG" "$LOG.1"
fi

# 3a. systemd (preferred, on the VPS):
#   sudo cp deploy/observe-campaign.service /etc/systemd/system/
#   sudo systemctl daemon-reload && sudo systemctl enable --now observe-campaign
#
# 3b. tmux + nohup (portable):
if command -v tmux >/dev/null 2>&1; then
  tmux new-session -d -s obs-o1 "nohup ./target/release/observe-campaign run >>$LOG 2>&1"
  echo "launched in tmux session 'obs-o1'. Attach: tmux attach -t obs-o1"
else
  nohup ./target/release/observe-campaign run >>"$LOG" 2>&1 &
  echo "launched with nohup (pid $!)"
fi
echo "status:   ./target/release/observe-campaign status"
echo "progress: tail -f reports/observation-o1-heartbeat.txt"
