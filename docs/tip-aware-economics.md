# Tip-aware economics (Prompt T, Part 1)

## Gate result: PROCEED

**meteora-dlmm ↔ pump-amm clears the infra floor by 100×–6,000× even at the p75
competitive-tip assumption.** The tip omission did **not** inflate the Prompt A
headline in the way Part 1 hypothesised — it turns out the measured tip floor is
one-to-three orders of magnitude *smaller* than the qualifying events, so
subtracting a competitive tip moves the economics by **< 0.2%**. Part 2 (scope
reduction) is authorised.

This directly contradicts the Part 1 premise ("if the true cost floor is near or
above 50k, a large share of events counted as opportunities are not profitable").
The data shows the opposite: the 50k threshold was, if anything, *conservative*
(too high), not too low. Reported as found.

---

## T1 — the 50k threshold is now one constant

`ADDRESSABLE_THRESHOLD_LAMPORTS = 50_000` had two definitions
(`forensics_batch.rs`, `forensics/campaign.rs`). Both now source it from
`arb_common::cost::ADDRESSABLE_THRESHOLD_LAMPORTS`, with a cross-crate guard
(`campaign::tests::legacy_threshold_matches_common`) matching the
`BINS_PER_ARRAY` / `MAX_BIN_PER_ARRAY` pattern. The Q4 ladder's 50k rung and the
sig-fee floor now reference the shared constants too — no magic numbers at call
sites. The constant's doc comment marks it **LEGACY and unjustified**, retained
only for this before/after.

## T2 — measured Jito tip floor (not assumed)

**Source (cited, not from memory):** Jito Low-Latency Txn Send docs,
`https://docs.jito.wtf/lowlatencytxnsend/` → REST endpoint
`https://bundles.jito.wtf/api/v1/bundles/tip_floor`, returning
`landed_tips_{25,50,75,95,99}th_percentile` (SOL) and `ema_landed_tips_50th`.
**Documented minimum tip: 1000 lamports** ("The minimum tips is 1000 lamports").

Sampled **n = 35** readings over **2026-08-30 19:48–20:17 UTC** (~28 min, one
reading / 45 s). Raw samples in `reports/tip-floor-samples.jsonl`. Each reading
is itself a percentile over recently *landed* tips; the table below is the
distribution of those percentile series across the 35 samples (lamports):

| tip percentile | median across samples | p25 | p75 | p95 | max |
|---|---|---|---|---|---|
| 25th landed | 1,780 | 1,201 | 2,240 | 7,214 | 10,089 |
| **50th landed** | **4,450** | 3,136 | 5,791 | 14,732 | 19,349 |
| **75th landed** | **16,782** | 9,264 | 36,228 | 202,486 | 501,343 |
| 95th landed | 213,007 | 100,000 | 500,000 | 1,128,188 | 5,277,594 |
| 99th landed | 997,650 | 417,290 | 1,184,534 | 4,075,219 | 9,007,884 |

**Competitive-bid assumptions used below (median across samples):**
p50 = **4,450 lamports**, p75 = **16,782 lamports**, p95 = **213,007 lamports**.

Sampling caveat: 28 minutes is a short window and it is a **global** tip floor
across all Jito bundles, not these specific hot markets — see Threats.

## T3 — explicit cost model

`arb_common::cost::min_profitable_gross_lamports` (tested):

```
net = gross_profit - tip - priority_fee - signature_fees
net > 0  ⇔  gross > tip + priority_fee + signature_fees
```

- **tip** — a *competitive bid*, not a fee; paid **only when the bundle lands**.
  A bundle that does not land costs nothing — the property that let the audited
  third-party bot lose 0.2103 SOL on 19,576 reverted *raw* transactions while
  this design cannot. Modelled at **both p50 and p75** (and p95 for the tail).
- **priority_fee** — ComputeBudget (cu_limit × cu_price).
- **signature_fees** — 5,000 × signatures.

**Accounting note that decides the whole analysis:** the forensics "value P&L"
is *realized net* from balance deltas, which already subtracts every cost the
landing operator paid **inside the arbitrage transaction** (priority, signature,
and any in-tx Jito-tip CPI). The only cost it could miss is a tip paid as a
**separate bundle transaction**, which the instrument cannot distinguish without
re-scanning (forbidden by T4). So the correction applied below subtracts the
competitive tip **on top of** the already-net event value — **conservative**: it
double-counts wherever the operator already tipped in-tx, i.e. the true picture
is at least this good.

## T4 — before/after from stored outcomes only (no re-scan, no RPC)

### Per-market (Prompt A strict batch outcomes; full rung tables)

Naive-vs-strict: all rows below are **strict** (external-inflow bias filter
armed). SOL/USD for these batch rows was the assumed $105.44 (Prompt A), flagged
there; the campaign rows below carry their own measured SOL/USD.

| market | ev ≥50k (old) | clearing 50k+p50-tip | clearing 50k+p75-tip | median event (lamports) | Σ≥50k (SOL) |
|---|---|---|---|---|---|
| CTPoyCwk | 9 | 9 | 9 | 4,710,167 (0.0047 SOL) | 0.0548 |
| A13oRB9F | 7 | 6–7 | 6–7 | 579,591 | 0.0489 |
| J4DMRf1c | 1 | 0–1 | 0–1 | 29,908 | 0.0001 |
| FZqdw6oS | 0 | 0 | 0 | — | 0.0000 |

The big markets' events (median 0.0047 SOL = 4.7M lamports) dwarf even the p95
tip (213k). The one marginal market (J4DMRf1c, median event 29,908 lamports) is
comparable to the tip and does not survive — but it contributes ~0.0001 SOL and
is irrelevant to the class total.

### Class-level (O1 campaign, strict, each sweep's own measured SOL/USD)

| sweep | SOL/USD (n) | class $/day (old) | after p50-tip | after p75-tip |
|---|---|---|---|---|
| 0 | n=20 | $10,545.14 | $10,542.33 | $10,534.54 |
| 1 | n=26 | $192.56 | $192.49 | $192.27 |
| 2 | n=33 | $576.72 | $576.38 | $575.42 |

The p75-tip correction is **−0.10% to −0.20%**. The tip is a rounding error at
this event scale.

**Every $/day figure here is heavily extrapolated** (windows 0.075h–3h scaled to
a day) and is **other operators' realized net, not profit we have shown we can
capture** — the same caveats as the O1 campaign. This section answers only the
narrow tip question, not capturability.

## T5 — gate: PROCEED

Infra floor $49/mo ≈ **$1.63/day**. At the **p75** tip assumption, every
conclusive sweep clears it with margin:

| sweep | after-p75 $/day | × daily infra floor |
|---|---|---|
| 0 | $10,534.54 | 6,463× |
| 1 | $192.27 | 118× |
| 2 | $575.42 | 353× |

Even the weakest sweep clears by 118×. **PROCEED to Part 2.**

## Threats to validity (stated plainly)

- **The tip floor is global, these markets are not.** The p50/p75 come from all
  Jito bundles; arbitrage on hot pump tokens competes in a higher-tip
  subpopulation. The p95/p99 tail (213k–998k lamports median, spiking to 5–9M)
  is the relevant regime under contention. At p95 (213k) the big markets still
  net positive (4.7M − 213k ≈ 4.5M; 579k − 213k ≈ 366k) but margins on
  mid-size events compress materially. This does not change the gate, but a
  build must bid against the *market-specific* tail, not the global p50.
- **Realized-net double-count.** The correction subtracts a tip on top of
  already-net events; if operators tipped in-tx, the true figures are better.
  The direction of the error is favourable, so it cannot flip PROCEED to STOP.
- **Extrapolation & non-stationarity.** Every $/day is a short window scaled up;
  sweep 0 ($10.5k/day) and sweep 1 ($192/day) differ 55× — the persistence
  question is the O1 campaign's, not settled here.
- **Not capturability.** These are other operators' realized profits. Nothing
  here shows we would win the auction or land the bundle.
- **B5 remains uncleared.** No engine-vs-`simulateTransaction` parity has been
  run; the O1 "parity capture" was never wired. Nothing in Part 1 touches this.
- **28-minute tip sample.** A longer sample spanning contention events would
  widen the tail estimate; the median percentiles are stable but the tail is
  under-sampled.
