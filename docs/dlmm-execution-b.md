# Prompt B — Meteora DLMM execution gap: status

Closes the "quotable but not executable" gap for `meteora-dlmm`, justified by
the Prompt A measurement (`docs/forensics-batch-a.md`: meteora-dlmm+pump-amm at
BUILD-level on two independent markets). Commit `b2c74fe`.

**Arming unchanged.** `Mode::Observe` stays default. No acceptance marker
created, no submit flag set. This work makes the venue *expressible*; it does
not arm it.

## B1 — ABI (done, tested)

`common/src/ix.rs`, strictly additive:
- `DexKind::MeteoraDlmm = 2` (serde `"meteora-dlmm"`), `parse_instruction` arm.
- `build_meteora_swap_data` — swap2 disc + amount_in + min_amount_out + empty
  `remaining_accounts_info` (`00000000`).
- `METEORA_SWAP2_DISCRIMINATOR`, `METEORA_DLMM_PROGRAM_{STR,ID}`, base58-guarded.
- **Wire format unchanged**: `HEADER_LEN=17`, `HOP_LEN=12`. DLMM's variable
  bin-array count rides the existing `num_accounts: u8`, proven by
  `dlmm_hop_fits_existing_wire_format` (a mixed DLMM→Whirlpool route round-trip).
- Cross-crate source guards in monitor assert the constants equal their source
  of truth (`meteora_reconstruct::SWAP2_DISCRIMINATOR`,
  `meteora_dlmm::DLMM_PROGRAM_ID`, and the fixture's disc string).

## B2 — on-chain CPI (done, tested)

`program/src/lib.rs`: `DexKind::MeteoraDlmm` branch, `METEORA_DLMM_PROGRAM`
constant, expected-program check, swap-data build. Privileges inherited
verbatim; no new signer seeds. Builds under `cargo build-sbf`.

## B3 — intermediate-minimum check (done, tested)

`gap-analysis.md` §F was open: the program checked only the final base balance,
so an intermediate leg could be sandwiched to near-zero and still pass. Now each
non-final hop's `min_amount_out` is enforced on-chain against the account the
**next** hop sweeps (its source = this hop's realized output), by re-reading the
SPL balance after the CPI — not by trusting the DEX's own slippage guard.

Two mollusk tests against the compiled `.so`:
- `two_hop_reverts_when_intermediate_below_min` — the final profit check would
  have passed; only the B3 check catches it (`ProfitNotMet`, code 8).
- `two_hop_passes_when_intermediate_clears_min` — no false revert.

## B4 — executor resolution (done, tested)

`executor/src/resolver.rs`: `MeteoraDlmmKeys` decode (offsets from the
6/6-live-exact monitor decoder) + `dlmm_hop` building the swap2 account list.

**Account order is triple-validated:**
1. Matches the captured mainnet CPI in `swap2_cpi_fixtures.json` indices 0–15,
   byte-for-byte.
2. Matches `sim_parity::build_dlmm_swap2_ix` — the builder already proven via
   `simulateTransaction` in Slice 5 (METEORA DIRECT PARITY PROVEN).
3. `dlmm_hop_account_order_matches_swap2_idl` asserts it in a unit test.

**Bin arrays come from the quote**, not a guess: `OpportunityHop` gained a
`bin_arrays: Vec<i64>` field (empty for Raydium/Whirlpool; skip-serialized so
existing JSON is unaffected). `dlmm_hop` **errors on an empty set** rather than
fabricate one — so the resolved accounts and the quoted output can never
disagree (the prompt's requirement). 4 resolver tests: account order, direction
reversal (input=y swaps the user ATAs), empty-bin-array refusal, foreign-mint
rejection.

**Single PDA source**: DLMM derivations moved to `arb_common::dlmm_pda` (feature
`pda`, off-chain only — the `no_std` program never pulls `solana-pubkey`).
`sim_parity` now delegates to it, so there is one definition, not two.

## B5 — parity before arming (HARNESS READY; LIVE RUN IS THE REMAINING GATE)

This is the arming gate and it is **not yet cleared**. Required bar (unchanged
from the prompt):

1. ≥ 20 captured real DLMM swaps replayed, local quote vs `simulateTransaction`,
   **max error ≤ 1 bps**.
2. A `Mode::Simulate` run over live opportunities with a build-success rate and
   the failure taxonomy written to `reports/failures.json`.

**What already exists** (so this is a run, not a build-from-zero):
- `meteora_dlmm::dlmm_quote_exact_in` — the local quote engine, already
  6/6 live-exact on stored bin prices.
- `sim_parity::build_dlmm_swap2_ix` — the exact swap2 instruction for
  `simulateTransaction`, whose account order B4 now shares.
- `SimRpc` + `SafetyGate` — the simulate-only RPC wrapper and the hard guard
  that refuses `ENABLE_SUBMIT`/`ENABLE_JITO` unless `MODE=simulate`.
- Slice 5 already proved DIRECT swap2 parity on route 1 (3 captured fixtures).

**What the run needs**: the current fixture set has **3** captured DLMM swaps,
not 20. Clearing B5 requires capturing ≥ 20 real swaps across the live
meteora-dlmm+pump-amm markets Prompt A surfaced, replaying each through the
existing quote-vs-simulate path, and recording the bps distribution + the
Simulate-mode failure taxonomy. That is a bounded live-RPC campaign, deliberately
left as an explicit, operator-run step because it is the gate immediately before
arming — it should not be auto-run inside a build session.

## Not covered by a test (stated plainly, per the prompt)

- **B5 in full** — the ≥20-swap ≤1 bps live parity and the Simulate-mode
  failure-taxonomy run. The pieces are in place and B4's account order is
  validated three ways, but the 20-swap live measurement has not been executed.
- **DLMM quoting in the discovery pipeline.** `resolve_hop` executes a DLMM hop,
  but the monitor's `Pool`/`quote_pool` discovery path does not yet emit
  `meteora-dlmm` routes with populated `bin_arrays`. Until it does, DLMM
  opportunities must be supplied with their bin-array set already attached
  (which is exactly what a DLMM-aware quote will produce). The resolver refuses
  anything less, so there is no silent-wrong path — only an explicit error.

## Gate

`cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets
-- -D warnings` clean; `cargo test --workspace` **297 passed / 0 failed**;
mollusk integration suite **9 passed / 0 failed** against the rebuilt `.so`.
