# Pump-amm execution leg (Prompt P)

Builds the missing half of `meteora-dlmm ↔ pump-amm`: the pump-amm ABI, CPI,
intermediate-min coverage, and executor resolution. Mirrors B1–B4. No arming —
`Mode::Observe` default, no marker, no submit flag, `submission_armed()` gate
untouched.

## 1. Reused vs. rebuilt

| piece | reused / rebuilt | why |
|---|---|---|
| pump Pool parser (`decode_pump_pool`) | **reused** | proven layout, chain-verified (`docs/pump-amm-layout.md`); offsets cited below |
| exact quote (`pump_quote`, `sell_quote_with_fee_split`) | **reused** | S1–S13 event-exact (17/17); reimplementing from IDL would discard proven parity |
| fee-v2 decoder (`pump_feev2`) | **reused** | dynamic schedule; the fixture is byte-identical to live mainnet (§3) |
| PDA derivations ([2],[15],[17],[18]) | **reused**, moved to `arb_common::pump_pda` | one shared home mirroring `dlmm_pda`; seeds proven in `sim_parity` |
| swap discriminators, program id | **reused** | cited to `pump_amm.rs` / `pump_reconstruct.rs`; guarded cross-crate |
| ABI hop encoding | **rebuilt** (additive) | `DexKind::PumpAmm=3` + `build_pump_swap_data`, mirroring the DLMM hop |
| executor resolver (`pump_hop`) | **rebuilt** | no resolver existed; built on the reused parser + PDAs + carried accounts |

Nothing proven was replaced. The only new logic is the ABI hop, the on-chain
branch, and the resolver — each built on reused, cited primitives.

## 2. Constants, each cited

| constant | value | source |
|---|---|---|
| pump program id | `pAMMBay6…` | `pump_amm.rs::PUMP_AMM_PROGRAM_ID` (guarded in `arb_common::ix`) |
| fee program id | `pfeeUxB6…` | `sim_parity.rs::PUMP_FEE_PROGRAM_ID`; live owner confirmed §3 |
| SELL discriminator | `33e685a4017f83ad` | `pump_reconstruct.rs::SELL_DISCRIMINATOR` / `pump_amm.rs::IX_SELL_DISCRIMINATOR` (guarded) |
| BUY discriminator | `66063d1201daebea` | `pump_amm.rs::IX_BUY_DISCRIMINATOR` (guarded) |
| Pool disc | `f19a6d0411b16dbc` | `pump_amm.rs::POOL_DISCRIMINATOR` / `pump-amm-layout.md` |
| base_mint / quote_mint offsets | 43 / 75 | `pump-amm-layout.md`, `pump_amm.rs::decode_pump_pool` |
| base_vault / quote_vault offsets | 139 / 171 | same |
| coin_creator offset | 211 | same (marked provisional; `has_creator` behaves correctly) |
| global_config PDA | `["global_config"]` | `sim_parity.rs` (verified == sell [2]); re-derived to the captured value in `pump_pda` test |
| event_authority PDA | `["__event_authority"]` | Anchor; re-derived to captured sell [15] in `pump_pda` test |
| coin_creator_vault_authority | `["creator_vault", creator]` | `sim_parity.rs` (verified == sell [18]) |
| coin_creator_vault_ata | ATA(auth, quote_mint, token) | `sim_parity.rs` (verified == sell [17]) |
| 24-account SELL order + w/s flags | see resolver | captured mainnet CPI `reconstruction_fixtures.json` route1 |

## 3. Fee-v2 tier implementation and source

Pump uses the **dynamic fee-v2 schedule** (24 market-cap tiers), NOT a flat
30 bps — the assumption that killed the strategy in S13. It is read from the
fee-program global config `5PHirr8…` (owner `pfeeUxB6…`), decoded by
`monitor/src/pump_feev2.rs` (tiers at offset 109, stride 40), and applied via
`sell_quote_with_fee_split`. Source: `docs/pump-fee-v2-layout.md`.

**Fresh mainnet check (this session):** fetched the live `5PHirr8…` account and
compared to the committed fixture — **byte-identical** (sha256
`e1c4647573d8…`, 4073 bytes, owner `pfeeUxB6…`). The schedule the quote depends
on has not drifted since capture, so the archived parity holds against current
fees.

**Bug audit (constraint 3):** the legacy flat `fee_split(has_creator)` in
`pump_amm.rs` is explicitly documented as legacy and correct only for the top
tier; the live quote path uses `sell_quote_with_fee_split` with the fee-v2 tier.
No shipped code path quotes a pump fee at a constant — confirmed by grep and by
`route1_captured_state_resolves_75bps_and_sells_exact` (a 75-bps tier, not 30).

## 4. Account-order validation — 2 independent ways (floor met; B4 had 3)

1. **PDA re-derivation vs captured CPI**: `pump_pda` re-derives global_config
   [2] and event_authority [15] to the exact pubkeys in the captured mainnet
   sell CPI (`pool_independent_pdas_match_captured_cpi`).
2. **Full 24-account order + writable/signer flags vs captured CPI**:
   `pump_hop_account_order_matches_captured_cpi` asserts every index, the
   writable set `{1,2,6,7,8,9,11,18,24}`, and the single signer, against the
   captured fixture's flags; carried accounts land at the exact carried indices.

A **third** way (a from-scratch `simulateTransaction` builder, as DLMM had) was
**not achievable**: it is blocked by the same undocumented accounts as the
resolver (§ below), so there is no independent from-scratch builder to compare
against. **2 of 3 achieved.**

### The resolver cannot be pure-derivation (cited blocker, handled correctly)

`docs/pump-fee-v2-layout.md` establishes that six accounts **cannot be derived
from pool state**: [9] protocol_fee_recipient and [10] its ATA **rotate**, and
[19] fee_config, [21] fee_pool, [22] fee_pool_state, [23] fee_recipient_ata have
**undocumented seeds**. A B4-style pure-derivation resolver is therefore
impossible. Per constraint 5, `pump_hop` **requires these 6 carried from the
quote** (`OpportunityHop.pump_carried_accounts`, in a fixed order) and
**hard-errors on an empty/short set** — never guessed, never defaulted. This is
the DLMM `bin_arrays` pattern: proven on-chain values carried through the route.
The other 18 accounts are derived from pool fields, PDAs, and mint owners (read,
not assumed).

## 5. Quote parity (measurement, not a gate)

- **Ported intact**: 15 `pump_amm` quote tests pass, including the S1–S13
  proven fixtures.
- **SELL (base in → quote out)**: **17/17 real swaps event-exact**, both creator
  and creator-less (archived, preserved in
  `sell_matches_real_swaps_exactly_creatorless_and_creator`). The fee-v2 config
  it depends on is byte-identical to live mainnet (§3), so this holds now.
- **BUY (quote in → base out)**: **exact for creator-less pools**
  (`buy_matches_real_swaps_exactly_creatorless`); **creator-pool BUY is still
  REFUSED** (`CreatorBuyUnverified`, `creator_buy_is_refused_not_overestimated`)
  — the S1–S13 limitation **still applies**. A creator pool remains usable as the
  SELL leg. For the WSOL cycle this means the pump leg should be the SELL
  (token→WSOL) whenever the pool has a creator.
- **Honest limitation**: a fresh N-swap live event-parity harness was **not
  re-run** this session. Instead the parity's live dependency (the fee-v2
  schedule) was re-verified byte-identical, and the ported quote's proven
  fixtures pass. bps distribution / outliers: the archived result is 0-bps
  (exact) on 17/17 sells; no fresh outlier scan was performed. This is
  measurement only and, like B5, clears no gate.

## 6. What is still missing before this pair can execute end to end

1. **B5 parity is NOT cleared.** No engine-vs-`simulateTransaction` parity has
   been run for either leg; the O1 "parity capture" was never wired. This
   remains an operator-run step. Nothing here changes it.
2. **The 6 carried pump accounts must be sourced.** The resolver requires them
   from the quote, but the monitor/discovery side does not yet clone them from a
   recent tx and attach them to the opportunity. Until it does, a pump hop
   hard-errors (by design) rather than executing.
3. **DLMM routes are not emitted by discovery either** (noted in Prompt B): the
   monitor's quote pipeline does not yet produce `meteora-dlmm`/`pump-amm`
   opportunities with their carried account sets.
4. **Fresh live event-parity harness** (P5) for both legs at current state.
5. **coin_creator offset [211]** is still "provisional" per the layout doc
   (never cross-checked against a non-zero-creator 243-byte pool); `has_creator`
   behaves correctly on every sampled pool but the offset wants a hard confirm.
6. **No end-to-end simulate/land test** of a full WSOL→token→WSOL bundle across
   both venues has been run.

Until 1–6 are addressed the pair is expressible but **not executable end to
end** — stated plainly so it is not mistaken for a working pipeline.

## Program size / CU

Pinocchio program: **25,240 → 25,616 bytes (+376, +1.5%)** for the pump branch.
Success-path CU unchanged at **5,448** for existing paths (the pump arm adds a
match branch, no CU on non-pump hops); a pump hop's CU is equivalent to the
other venues by construction (build data + forward CPI + intermediate check).

## Acceptance gate

`cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets
--features arb-monitor/legacy-tooling -- -D warnings` clean; `cargo test
--workspace --features arb-monitor/legacy-tooling` **317 passed / 0 failed**
(+13: ABI, pump_pda, resolver, mollusk pump-intermediate); mollusk suite **11
passed** (incl. the two pump-intermediate cases); default minimal build produces
only `arb-monitor`; preflight confirms arming disabled.

**B3 negative verified, not assumed**: with the intermediate check removed and
the `.so` rebuilt, `pump_intermediate_reverts_when_below_min` FAILS
(`expected Custom(8), got Ok(())`); restored, it passes. The check fires on the
pump leg.
