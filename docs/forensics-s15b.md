# S15B — Historical forensics on `meteora-dlmm + orca-whirlpool`

READ-ONLY. Historical RPC only. No Geyser, no streaming subscription, no
transaction construction, no signing, no submission, no executor code, no build
decision. Tool of record: `monitor/src/bin/forensics_s15b.rs`; input fixture
`monitor/fixtures/forensics/s15b_input.json` (committed `2bc29b8`).

Window: slots 434673538–435131694, **53.41 h**.

---

## Verdict table

| # | question | verdict | the number that decides it |
|---|---|---|---|
| Q1 | same-slot backrun or cross-slot? | **PASS — decisively** | CROSS_SLOT 40/45; gap median **2 slots (~800 ms)**, p90 33 (~13 s), max 51 (~20 s) |
| Q2 | public flow or staked connections? | **PASS** | 36 distinct leaders / 45 blocks vs stake-weighted null median 37, *p* = 0.35 |
| Q3 | business or lottery? | **MIS-SPECIFIED** | measured **land rate** 98.1%, not win rate; subjects are buyers, not arbitrageurs |
| Q4 | frequency + economics? | **KILL** | 29.2 events/day ≥ 50k lamports, worth **0.0043 SOL/day ≈ $9.76/month** for *all* participants |

**Q4 is the question that killed it.**

## Recommendation

# CLOSE THE PROGRAMME

The pools are **uncontested and reachable** — and **empty**. Q1, Q2 and the
participant census all came back favourable. It does not matter. The largest
single arbitrage in 53.41 hours was worth **$0.14**.

---

## Q1 — the sub-slot premise is falsified **PASS** (n = 45)

| class | count | share |
|---|---|---|
| CROSS_SLOT | 40/45 | 89% |
| SAME_SLOT_BACKRUN | 5/45 | 11% |

Slot gap (arb slot − last preceding pool touch): min 0, **median 2 (~800 ms)**,
p90 **33 (~13 s)**, max **51 (~20 s)**. gap ≥ 1: 40/45. gap ≥ 2: 31/45.

**These pools are not contested. Nobody is racing for them.** A dislocation
typically sits for the better part of a second, and in the tail for *twenty
seconds*, before anyone takes it. Combined with Q3's 98.1% land rate — near-zero
revert means participants are not colliding — the picture is of a quiet corner
of the market, not a hot one.

**This falsifies the sub-slot premise that drove the last several sessions.**
The working assumption since S15A was that this edge lives for a fraction of a
slot and therefore demands ShredStream, leader-adjacent placement, or
co-location. It does not. Geyser at `processed` commitment is comfortably fast
enough, with an order of magnitude of headroom against the median. **The
infrastructure question was never the binding constraint** — which is exactly
why the infrastructure answer could not have rescued this.

**Censoring caveat (important, and it cuts in our favour):** the gap is measured
only on opportunities that were *captured*. Dislocations that no one ever took
are invisible to this measurement, and they are precisely the long-lived ones.
So the true lifetime distribution has a **longer** tail than reported here,
never a shorter one. p90 = 33 slots is a floor on the tail, not a ceiling.

Two controls, both required before the number is believable:

1. **Self-match control.** The arb must match its own pools under the exact
   extraction used to find a trigger; 6/6 verified, now a hard error in the
   binary. Without it, unresolved ALT-loaded keys would silently produce
   CROSS_SLOT for all 45 (45/45 use ALTs).
2. **Leader source.** From block `rewards` (`Fee` recipient), not
   `getSlotLeaders`, which is only defined for the current epoch and most of
   these slots predate it. Cross-validated **15/15** on in-epoch slots.

An earlier implementation searched only *within* the arb's own block and called
"no preceding toucher" CROSS_SLOT. Unsound: these pools are quiet enough that
the arb is usually the **only** transaction in its ~1,000-transaction block
touching them, so that test cannot separate "trigger one slot earlier" from "no
trigger for 51 slots". Same verdict, different evidence.

## Q2 — public flow **PASS** (n = 45 blocks)

Null: producers appear in proportion to activated stake (710 validators,
428.8 M SOL). 20,000 Monte-Carlo trials.

| statistic | observed | null median | null 5–95% | *p* |
|---|---|---|---|---|
| distinct leaders | 36 | 37 | 33–41 | 0.348 |
| max blocks by one leader | 3 | 3 | 2–5 | 0.799 |
| blocks in top-3 | 9/45 | — | — | 0.280 |

**Power:** detects single-validator routing of *f* ≥ 20% with 100% probability,
*f* = 10% with 1%. Rules out strong privileged routing; cannot exclude a mild
tilt. Corroborated by 0 Jito tips across all 45.

A trap avoided: ranking the top-10 leaders by observed count and dividing by
stake share showed apparent 10–12× over-representation. That is a selection
artefact — conditioning on a high count inflates the ratio for any low-stake
validator. The whole-distribution test above is the valid one.

## Q3 — mis-specified, and reframed

**What was measured is a land rate, not a win rate.** The 4 fixture signers
submitted **1,254** transactions in the window and landed **1,230** (**98.1%**,
24 reverts).

| signer | in-window | landed | reverted | land rate |
|---|---|---|---|---|
| `2bLXQjWt…` | 179 | 176 | 3 | 98.3% |
| `591jWVDk…` | 275 | 262 | 13 | 95.3% |
| `Bq23RjEh…` | 374 | 373 | 1 | 99.7% |
| `HQWuDd7p…` | 426 | 419 | 7 | 98.4% |

A win rate is *profitable attempts ÷ attempts*. This is *confirmed transactions ÷
submitted transactions* — a submission-quality metric. It says these wallets
rarely revert, which is true of ordinary swaps and says nothing about
arbitrage.

**Operators' actual capture rate:** of the 7,555 landed cross-venue
transactions, the 4 fixture signers account for **57 = 0.75%**
(`HQWuDd7p` 32, `591jWVDk` 20, `2bLXQjWt` 5, `Bq23RjEh` 0).

Their arbitrage capture rate is **0**, because — see below — they were not
arbitraging. The 98.1% is real and irrelevant.

## Q4 — frequency, distribution, economics **KILL**

### Participant census (the decisive unmeasured number)

| quantity | value |
|---|---|
| distinct txs touching the 6 target pools in 53.41 h | 151,054 |
| touching ≥1 Meteora **and** ≥1 Whirlpool pool | 7,942 |
| landed | 7,558 |
| accounted individually (full enumeration, no sampling) | 7,555 (3 RPC misses) |
| **distinct signers across those 7,555** | **2,207** |
| top-1 / top-5 / top-10 share of transactions | 7.0% / 10.1% / 12.0% |
| Gini (transaction counts) | 0.583 |

**2,207 distinct signers with the busiest holding 7.0% is a retail crowd, not a
field of competing bots.** This is the strongest single piece of evidence that
the 7,555 cross-venue transactions are overwhelmingly *user swaps routed across
both venues* (Jupiter-style splits), not arbitrage cycles.

Narrowing to actual extraction: **137** distinct signers take any positive
value; **23** take anything above the economic threshold below. So on the
participant axis the answer is genuinely "neglected pools with room" — there is
no crowd to out-run. The pot is simply empty.

### Accounting

Value P&L = `dSOL + dWSOL + dUSDC / P(slot)`, where `P` is the **local** market
rate: median implied rate of the 151 nearest clean two-asset SOL↔USDC swaps by
slot. The price is derived from the population rather than assumed — the window
is historical, and the rate drifted 73 → 78 across 53 h, enough to swamp a thin
margin. Window-wide: **74.948 USDC/SOL** (n = 3,545, IQR 73.86–76.65).

Priceable: **4,428** of 7,555. The other **3,127** carry non-zero deltas in
other mints (directional multi-token routes); exactly **0** of them are cycles
through another token, so none is a missed arbitrage of the shape we build.

### Profit concentration (n = 645 value-positive)

| slice | share of all profit |
|---|---|
| top 1% (6 txs) | 30.7% |
| top 5% (32 txs) | 48.9% |
| top 10% (64 txs) | 61.8% |
| top 25% (161 txs) | 82.6% |
| top 50% (322 txs) | 95.2% |

Tail-dominated, which is why the total is a misleading statistic and the
threshold table below is the right one.

### Events above an economic threshold

Value P&L is **already net of the fee the operator paid** (`dSOL` is
post−pre on the fee payer, and the fee is debited there). Assumptions stated:
one signature (5,000 lamports base), no Jito tip (0 observed across all 45),
priority fee as actually paid by the capturing party.

| net threshold (lamports) | events | events/day | SOL/day | distinct signers |
|---|---|---|---|---|
| 0 | 645 | 289.8 | 0.00699 | 137 |
| 5,000 (1-sig fee floor) | 392 | 176.1 | 0.00682 | 84 |
| 10,000 | 260 | 116.8 | 0.00644 | 62 |
| 25,000 | 148 | 66.5 | 0.00564 | 35 |
| **50,000** | **65** | **29.2** | **0.00434** | **23** |
| 100,000 | 18 | 8.1 | 0.00287 | 11 |
| 250,000 | 5 | 2.2 | 0.00204 | 5 |

At the **≥ 50,000 lamport** threshold: 65 events in 53.41 h = **29.2/day**, one
every ~49 minutes, median 76,283 lamports, max 1,828,512, captured by 23
distinct signers.

**39% of all value-positive events (253/645) fall below the 5,000-lamport
signature-fee floor**, and the median value-positive event is 5,890 lamports —
barely one signature above breakeven.

### Economics — where it dies

The threshold is the right frame, and it still fails, because **50,000 lamports
is $0.0037**:

| item | value |
|---|---|
| above-threshold extraction, **all 23 participants combined** | 0.00434 SOL/day = 0.130 SOL/month |
| at 74.95 USDC/SOL | **≈ $9.76/month** |
| median above-threshold event | 76,283 lamports = **$0.0057** |
| largest single opportunity in 53.41 h | 1,828,512 lamports = **$0.14** |
| Geyser floor, ~90 accounts (6 pairs × [1 pool + ~14 bin/tick arrays]) | **$49–200/month** |
| revenue ÷ cheapest tier at **100% capture of every above-threshold event** | **0.20×** |

Winning *every* qualifying event, against all 23 current participants, pays a
fifth of the cheapest subscription. Dropping the threshold to zero and taking
every positive lamport in the market yields $15.70/month — still 0.32×. There
is no capture rate, no latency improvement and no threshold choice that closes
this, because the constraint is not capture, latency or selection. **The pot is
smaller than the bill by an order of magnitude.**

### Q4(a) — threshold *swap size*: NOT RUN

The spec asked for the swap size that opens a dislocation ≥ median realized
profit, via the exact quote engines. Deliberately not run: it is a *proxy* for
the opportunity population, and the direct enumeration above measures that
population outright. Computing it against a median realized profit that is an
accounting artefact would yield a precise number about nothing.

---

## The finding that matters most: the evidence base was an artefact

**All 45 transactions in the fixture are ordinary purchases of SOL. 45/45
BUY-shaped, 0/45 cycles.**

`net_profit_lamports` was computed as `signer SOL delta + signer WSOL delta`.
That identity holds **only for a closed cycle** ending with the same non-SOL
inventory it started with. When a signer *spends* another token to acquire SOL,
it books the sale proceeds as profit and ignores what was paid.

Worked example — `5KTk2eUJya…`, every balance change:

| account | asset | delta |
|---|---|---|
| signer `HQWuDd7p…` | native SOL | **+9,747,693** |
| signer `HQWuDd7p…` | USDC | **−740,000** |
| 5 pool vaults | WSOL | −9,918,407 |
| 5 pool vaults | USDC | +734,006 |
| `BEochrdGhm5p…` | USDC | +5,994 (≈0.81% router fee) |

0.74 USDC spent across five pools for 0.00992 SOL — implied **74.61 USDC/SOL**
against a contemporaneous market of **73.25**, i.e. a purchase ~1.9% *over*
market plus a router fee. A retail aggregator buy. The "+0.0097 SOL profit" is
the SOL they bought.

| | value |
|---|---|
| fixture "realized net" (18 priceable txs) | **+0.082366 SOL** |
| true value P&L | **−0.002154 SOL** |

All 18 individually negative; the other 27 were fetched separately and are
identically BUY-shaped.

This also explains the "tight, non-lottery profit distribution, median ≈ p50 ≈
0.0025 SOL" that S15A read as proof of a mechanical operation. It is the
signature of a **fixed-size recurring buy**.

### Documents invalidated

- `docs/forensic-route-recon-s15a.md` — the 0.651 SOL headline and the
  multi-operator "working strategy" conclusion.
- `docs/xdex-corrected-measurement-s15a.md` — the "~1,000× gap" and the claim
  that polling cannot measure this class.

### Why the poller returned 0/125 — corrected

Two independent reasons, neither of them instrument failure:

1. **Base rate.** The value-positive population is 645 in 53.41 h — one per
   ~298 s. In the 180 s `--pairs` window, the expected number of events was
   **≈0.6** (above the 50k threshold, ≈0.06). Not 0.04, and not 7.
2. **Size.** The median opportunity is 5,890 lamports and 39% fall below the
   signature-fee floor, so even a detected event is mostly unbankable.

The observe run measured *gross* edge, with a best case of −2,073 lamports — so
at poll time there was usually no dislocation at all. **The 0/125 result was a
correct measurement of a market that is genuinely flat almost all of the time.**

---

## What was established

1. The pools are **reachable** (Q1: median 800 ms, tail 20 s) and the flow is
   **public** (Q2). The sub-slot premise is false.
2. The pools are **uncontested**: 2,207 signers but only 23 taking anything
   above threshold, and near-zero reverts.
3. Every favourable condition holds, and it is still a KILL: **29.2
   qualifying events/day worth $9.76/month across all participants**, against a
   $49/month floor.

This is the honest shape of the result: **we could reach this opportunity and we
could win it. It is not worth winning.**

## Correction to my own prior conclusions

In `docs/xdex-corrected-measurement-s15a.md` I wrote that the sub-slot diagnosis
"was more correct than I credited" and that polling "cannot measure this class
of opportunity at any interval". **Both withdrawn.** Q1 shows the opposite: the
opportunity is slow. The poller was right; I overrode a correct measurement with
a fabricated one and built three slices on it.

The failure was not instrument choice. It was **accepting a profit number
without validating the accounting identity that produced it**, then treating my
own re-derivation as corroboration when both used the same wrong formula.

Three measurement errors were caught during S15B itself — a block-local Q1
search, a 37-pool sweep with silent pagination truncation, and an
inventory-neutral cycle condition that rejected 18/18 known operator
transactions. **Every one surfaced through contradiction with another number,
none through re-reading the code.**

## Reproduction

```bash
cargo run -p arb-monitor --bin forensics-s15b -- --q1
```

Gate: `cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets
-- -D warnings` clean; `cargo test --workspace` **263 passed / 0 failed**.
