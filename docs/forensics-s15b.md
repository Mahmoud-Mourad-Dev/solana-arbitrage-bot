# S15B — Historical forensics on `meteora-dlmm + orca-whirlpool`

READ-ONLY. Historical RPC only. No Geyser, no streaming subscription, no
transaction construction, no signing, no submission, no executor code, no build
decision. Tool of record: `monitor/src/bin/forensics_s15b.rs`; input fixture
`monitor/fixtures/forensics/s15b_input.json` (sha256
`9c17d2cf…c5ca`, committed `2bc29b8`).

Window: slots 434673538–435131694, **53.41 h**.

---

## Verdict table

| # | question | verdict | the number that decides it |
|---|---|---|---|
| Q1 | same-slot backrun or cross-slot? | **PASS** | CROSS_SLOT **40/45** (89%); slot gap median **2**, p90 33, max 51 |
| Q2 | public flow or staked connections? | **PASS** | 36 distinct leaders / 45 blocks vs stake-weighted null median 37, *p* = 0.35 |
| Q3 | business or lottery? | **INVALID** | 98.1% land rate (1,230/1,254) — but the subjects are not arbitrageurs |
| Q4 | frequency + economics? | **KILL** | total extraction ≤ **0.0070 SOL/day** across *all* participants vs a **$49/mo** infra floor |

**Q4 is the question that killed it.** Per the stop rule, no further questions
were run after it returned KILL — Q1–Q3 had already completed.

## Recommendation

# CLOSE THE PROGRAMME

Not "buy Geyser and measure". The instrument was never the binding constraint.

---

## The finding that matters most: the evidence base was an artefact

**All 45 transactions in the S15B input fixture are ordinary purchases of SOL,
not arbitrages. n = 45/45. Zero are cycles.**

The fixture's `net_profit_lamports` was computed as
`signer SOL delta + signer WSOL delta`. That formula is correct **only for a
closed cycle** that begins and ends in the same asset. It silently treats the
proceeds of *selling another token* as profit.

Worked example — `5KTk2eUJya…`, every balance change in the transaction:

| account | asset | delta |
|---|---|---|
| signer `HQWuDd7p…` | native SOL | **+9,747,693** |
| signer `HQWuDd7p…` | USDC | **−740,000** |
| 5 pool vaults | WSOL | −9,918,407 (total) |
| 5 pool vaults | USDC | +734,006 (total) |
| `BEochrdGhm5p…` | USDC | +5,994 (≈0.81% router/platform fee) |

The signer spent 0.74 USDC across five pools and received 0.00992 SOL, which was
unwrapped to native SOL. Implied execution **74.61 USDC/SOL** against a
contemporaneous market of **73.25** — they paid ~1.9% *over* market, plus a
router fee. This is a retail-style aggregator **buy**, split across five pools.
The "+0.0097 SOL profit" is simply the SOL they bought.

Valuing the token leg at the local market rate for all 18 fixture transactions
that are priceable in WSOL/USDC alone:

| | value |
|---|---|
| fixture "realized net" (SOL+WSOL only) | **+0.082366 SOL** |
| true value P&L (token leg priced) | **−0.002154 SOL** |
| ratio | **−2.6%** |

All 18 are individually negative. The remaining 27 were fetched separately: all
27 have the identical shape (spends a token, gains SOL, gains no token). **45/45
BUY-shaped, 0/45 cycles.**

This also explains the observation S15A read as proof of a mechanical operation:
a "tight, non-lottery profit distribution, median ≈ p50 ≈ 0.0025 SOL". That is
the signature of a **fixed-size recurring buy**, not of arbitrage. Four wallets
buying SOL every ~10 minutes produce exactly that constant under one-sided
accounting.

### Documents invalidated by this

- `docs/forensic-route-recon-s15a.md` — "realized, repeated, multi-operator
  profit at a median of ~0.0025 SOL per trade" and the 0.651 SOL headline.
- `docs/xdex-corrected-measurement-s15a.md` — the "~1,000× gap between what a
  2-second poller sees and what is actually being earned", and the conclusion
  that polling cannot measure this class. The poller was right: **there was
  almost nothing there to see.** The 0 / 125 gross-positive result and the
  −2,073 lamport best edge were **correct measurements**, not instrument failure.

The S14B-3 archive decision was therefore **right, for a reason I had rejected**.
I re-opened it on the strength of a profit number my own pipeline manufactured.

---

## Q1 — same-slot or cross-slot? **PASS** (n = 45)

Method: for each arb, locate it in its block via `getBlock`
(`rewards: true`, `transactionDetails: Full`, `maxSupportedTransactionVersion: 0`),
then find the nearest **preceding** transaction touching any of its pools
**across block boundaries**, via each pool's signature history anchored at the
arb's own signature.

| class | count | share |
|---|---|---|
| CROSS_SLOT | 40/45 | 89% |
| SAME_SLOT_BACKRUN | 5/45 | 11% |

Slot gap (arb slot − last preceding pool touch), n = 45: min 0, **median 2**,
p90 33, max 51. gap ≥ 1: 40/45. gap ≥ 2: 31/45.

Two correctness controls, both required before this number can be believed:

1. **Self-match control.** The arb must match its *own* pools under the exact
   extraction used to search for a trigger; 6/6 verified, and it is now a hard
   error in the binary. Without it, unresolved ALT-loaded account keys would
   silently produce CROSS_SLOT for every transaction (45/45 of these use ALTs).
2. **Leader source.** Taken from block `rewards` (`Fee` recipient), not
   `getSlotLeaders`, which is only defined for the current epoch and most of
   these slots predate it. Cross-validated **15/15 agreement** on in-epoch slots.

An earlier implementation searched only *within* the arb's own block. That was
unsound: these pools are quiet enough that the arb is usually the **only**
transaction in its ~1,000-transaction block touching them, so "no in-block
trigger" cannot distinguish "trigger one slot earlier" from "no trigger for 51
slots". Same verdict, different evidence.

## Q2 — public flow or staked connections? **PASS** (n = 45 blocks)

Null hypothesis: block producers appear in proportion to activated stake
(710 validators, 428.8 M SOL total). 20,000 Monte-Carlo trials.

| statistic | observed | null median | null 5–95% | *p* |
|---|---|---|---|---|
| distinct leaders | 36 | 37 | 33–41 | 0.348 |
| max blocks by one leader | 3 | 3 | 2–5 | 0.799 |
| blocks in top-3 leaders | 9/45 | — | — | 0.280 |

No departure from stake-weighted assignment. 0/45 leaders were absent from the
current vote-account set.

**Power (stated because "no evidence" is meaningless without it):** against the
alternative that a fraction *f* of transactions is routed through one privileged
validator, this test detects *f* ≥ 20% with 100% probability but *f* = 10% with
only 1%. So it **rules out strong privileged routing and cannot rule out a mild
tilt.** Corroborated by 0 Jito tips across all 45 (prior scan).

A note on a trap avoided: ranking the top-10 leaders by observed count and
dividing by stake share produced apparent 10–12× over-representation. That is a
selection artefact — conditioning on a high observed count inflates the ratio for
any low-stake validator. The whole-distribution test above is the valid one.

## Q3 — business or lottery? **INVALID as specified**

Measured: the 4 fixture signers submitted **1,254** transactions in the window
and landed **1,230** (**98.1%**; 24 reverts).

| signer | in-window | landed | reverted | land rate |
|---|---|---|---|---|
| `2bLXQjWt…` | 179 | 176 | 3 | 98.3% |
| `591jWVDk…` | 275 | 262 | 13 | 95.3% |
| `Bq23RjEh…` | 374 | 373 | 1 | 99.7% |
| `HQWuDd7p…` | 426 | 419 | 7 | 98.4% |

This is a genuine measurement of a meaningless quantity: **these four wallets are
buyers, not arbitrageurs**, so a 98.1% land rate says only that ordinary swaps
succeed. It is not an arbitrage win rate and must not be cited as one. A valid
win rate would require identifying a competitor's *failed cycles*, which
presupposes cycles that this venue pair does not exhibit at any material rate.

## Q4 — frequency and economics? **KILL**

### Population (full enumeration, no sampling)

The 6 target pools are the validated pairs: Meteora `3PyikuAr`, `5XRqv7LC`,
`FbkX1h2Y`; Whirlpool `83v8iPyZ`, `BSddxwYW`, `Esvfxt3j`.

| quantity | value |
|---|---|
| distinct txs touching the 6 pools in 53.41 h | 151,054 |
| touching ≥1 Meteora **and** ≥1 Whirlpool pool | 7,942 |
| of those, landed | 7,558 |
| fetched and accounted individually | 7,555 (3 RPC misses) |
| priceable in WSOL/USDC alone | 4,428 |
| involving other mints (not priceable here) | 3,127 |

### Accounting

Value P&L = `dSOL + dWSOL + dUSDC / P(slot)`, where `P` is the **local** market
rate: the median implied rate of the 151 nearest clean two-asset SOL↔USDC swaps
by slot. Deriving the price from the population rather than assuming one both
avoids a present-day price (the window is historical) and controls for drift —
the rate moved 73 → 78 across 53 h, enough to swamp a thin margin.

Window-wide clean price: **74.948 USDC/SOL** (n = 3,545, IQR 73.86–76.65).

This choice also fixes the arb/user discriminator for free: a user swapping at
market prices to ≈0 value gain and drops out, while genuine extraction stays
positive. My earlier discriminator — requiring inventory neutrality — was wrong
in the opposite direction: it rejected 18/18 known operator transactions and
produced a 0.0083 SOL total, detectable only because it contradicted the
0.0824 SOL those same transactions reported. Neither figure survives.

### Result

| metric | value | n |
|---|---|---|
| value-positive transactions | 645 | of 4,428 priced (14.6%) |
| **total extraction** | **0.0156 SOL / 53.41 h** | 645 |
| **per day** | **0.0070 SOL/day** | — |
| median | 5,890 lamports (0.0000059 SOL) | 645 |
| mean | 24,114 lamports | 645 |
| max | 0.001829 SOL | 645 |
| distinct value-positive signers | 137 | — |
| top-1 share | 43.3% | — |

**0.0070 SOL/day is an upper bound**, and a generous one: it counts every
positive as real, when the median (0.0000059 SOL ≈ $0.0004) sits below the noise
floor of the price estimate. The true figure is lower.

### Economics

Compared like-for-like — total market revenue against total infrastructure cost,
sum vs sum, not a best case against a median:

| item | value |
|---|---|
| total extraction, **all 137 participants combined** | 0.0070 SOL/day = **0.21 SOL/month** |
| at 74.95 USDC/SOL | **≈ $15.70/month** |
| Geyser floor, ~90 accounts (6 pairs × [1 pool + ~14 bin/tick arrays]) | **$49–200/month** |
| revenue ÷ cheapest infra tier, at **100% capture of the entire market** | **0.32×** |

Capturing **every lamport every other participant earns** pays for less than a
third of the cheapest subscription — before hardware, RPC overage, capital, or
the 43.3% already taken by the top extractor. No capture rate closes this gap,
because the gap is not a capture-rate problem.

### Q4(a) — threshold swap size: **NOT RUN**

The spec asked for the threshold swap size opening a dislocation ≥ median
realized profit, via the exact quote engines. Not run, deliberately: it is a
*proxy* for the opportunity population, and the direct measurement of realized
extraction above is strictly stronger evidence and already returns KILL.
Computing a threshold size against a median realized profit that has been shown
to be an accounting artefact would produce a precise number about nothing.

---

## What was actually established

1. Dislocations on this venue pair **do** survive across slots (Q1) and the flow
   **is** public (Q2). Both hypotheses that would have justified buying
   infrastructure are **false** — and it does not matter, because
2. there is **almost nothing to extract**: ≤0.0070 SOL/day for the entire market,
   against a $49/month floor.
3. The 0.651 SOL that motivated this entire investigation **was never earned**.
   It is one-sided accounting over 45 ordinary SOL purchases.

## Correction to my own prior conclusions

In `docs/xdex-corrected-measurement-s15a.md` I wrote that the director's
sub-slot-latency diagnosis "was more correct than I credited" and that polling
"cannot measure this class of opportunity at any interval". Both statements are
withdrawn. The polling observer was measuring correctly; the 0/125
gross-positive result was the true answer. I overrode a correct measurement with
a fabricated one and then built three slices on top of it.

The failure was not instrument choice. It was **accepting a profit number
without validating the accounting identity that produced it** — and then
treating agreement between the fixture and my own re-derivation as
corroboration, when both used the same wrong formula.

## Reproduction

```bash
cargo run -p arb-monitor --bin forensics-s15b -- --q1
```

Gate: `cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets
-- -D warnings` clean; `cargo test --workspace` **263 passed / 0 failed**.
