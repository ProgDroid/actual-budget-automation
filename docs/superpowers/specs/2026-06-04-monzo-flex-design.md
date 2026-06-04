# Phase 3 (second half) — Monzo Flex Design

**Date:** 2026-06-04
**Status:** Approved (design); implementation pending
**Supersedes:** the inter-account (`p2p_payment`) slice originally planned as Tasks 10–13 in `2026-06-04-transfers.md`. The user does **not** do current→current p2p transfers; the real second-half need is **Monzo Flex**. Those p2p tasks are obsolete.
**Builds on:** the merged pots slice — reuses `TransferTarget::Account`, the transfer-payee import path, and the `held_transfers` durable queue.

## Goal

Import Monzo Flex (BNPL/credit) activity into Actual correctly, with Flex modelled as its own Actual account (already set up by the user), and **without double-counting** the lines that appear in both the main current-account feed and the Flex-account feed.

## Real-data evidence (user's redacted feed, 2026-06-04)

`monzo_flex_id` is **constant** across all transactions = the Flex *account* id (`monzoflex_0000…`), NOT a per-transaction key. Every `monzo_flex` line is a **two-sided mirror** (equal-and-opposite, one per feed).

| Scenario | Main-account feed | Flex-account feed | Pair key |
|---|---|---|---|
| **Repayment** | `scheme=monzo_flex`, amount **−82822**, `flex_transaction_type=repayment` | `scheme=monzo_flex`, amount **+82822**, `flex_transaction_type=repayment` | abs amount + date |
| **"Flex it"** (move existing purchase) | `scheme=monzo_flex`, amount **+5900**, `flex_transaction_type=transaction_moved_to_flex_current_account_side`, `original_transaction_id` | `scheme=monzo_flex`, amount **−5900**, `flex_transaction_type=transaction_moved_to_flex_flex_side`, same `original_transaction_id` | `original_transaction_id` (or abs amount + date) |
| **Direct Flex purchase** | *(no line)* | `scheme=mastercard`, amount **−14997**, `metadata.is_flex_direct_virtual_card_tx=true` | n/a (single-sided, already handled) |

Key facts:
- The main-account `monzo_flex` line fully captures the money movement; the Flex-feed `monzo_flex` line is its mirror with **no unique economic information**.
- The **direct purchase** is `scheme=mastercard` (NOT `monzo_flex`), single-sided on the Flex feed → already flows through the existing Phase-1/2 pipeline and routes to the Flex Actual account.
- `counterparty` is `{}` on all `monzo_flex` lines (no payee — correct for a transfer).

## Scenario → Actual outcome

| Scenario | aba behavior | Actual result |
|---|---|---|
| Repayment | import the **main-side** `monzo_flex` line as a transfer → Flex account; **suppress** the Flex-side mirror | main −828.22 ↔ Flex +828.22 (one linked transfer) |
| "Flex it" | same: import main-side (+59.00) as transfer → Flex; suppress Flex-side mirror | main: original −59 + transfer +59 = **net 0**; Flex −59 (expense moved to Flex) |
| Direct Flex purchase | **existing path** — ordinary payee/category classification into the Flex account | Flex −149.97 categorised spending |

Net result over time: the Flex account accrues purchases (−) and repayments (transfers in, +); its balance = outstanding Flex debt. Correct double-entry.

## Design

### 1. Identify the Flex account (`aba accounts`)

- `list_accounts` (already returns `(id, description)`) gains a filter: **exclude** accounts whose `description` starts with `monzoflexbackingloan_` (the internal backing-loan account — it 403s on `/transactions`, which would fail-loud the whole run if mapped). They are never suggested, mapped, or fetched.
- `AccountLink` gains `#[serde(default)] pub is_flex: bool`. During `suggest_links` / `map_accounts`, set `is_flex = true` for the account whose `description` starts with `monzoflex_`. User-overridable in the state file.
- At most one Flex account is expected; if several match, all are flagged `is_flex` (each is its own Flex account with its own mirror feed — the logic is per-account-id, so this still works).

### 2. Classify `monzo_flex` lines (Monzo fetch)

`fetch_account(monzo_id, …)` knows which account it is fetching and has `self.monzo.accounts` (now with `is_flex`). For a line with `scheme == "monzo_flex"`:
- **fetched from a non-Flex account** (the main side) → `transfer = Some(TransferTarget::Account(flex_monzo_id))`, where `flex_monzo_id` is the `is_flex` account's `monzo_id`. (Reuses the existing Account-transfer path: resolve → Flex Actual account → its transfer payee.)
- **fetched from the Flex account** (the mirror side) → `transfer = Some(TransferTarget::FlexMirror)` (new marker variant; see §4).

If there is **no** `is_flex` account configured, a `monzo_flex` line cannot be routed → it is held (fail-loud) via the existing held queue (main side) / held as an unpaired mirror (§3). `mastercard` and all other schemes are unaffected (ordinary path).

### 3. Suppress the mirror — pair-else-hold (fail-loud)

Mirror suppression happens in `run()` after both feeds are fetched (so pairing can see both sides). Among the partitioned transfer rows:
- main-side `Account(flex)` lines → imported as transfers (existing path; held if Flex unmapped).
- `FlexMirror` lines → **suppressed iff paired** with a main-side `Account(flex)` line this run (match on **abs amount + date**; `original_transaction_id` is an exact key for "flex it" but amount+date is sufficient and uniform). A paired mirror is dropped (it carries no unique data; Actual recreates that side from the transfer).
- An **unpaired** `FlexMirror` line → **held** in `held_transfers` (fail-loud), surfaced in the run summary as "Flex line awaiting review" — never silently dropped. Rationale: a one-sided `monzo_flex` line on the Flex feed would be unexpected (e.g. interest/fees, whose representation is unconfirmed); holding surfaces it for inspection instead of guessing. **OPEN ITEM to validate against real data: do Flex interest/fee charges appear as one-sided `monzo_flex` lines? If so, a follow-up will classify them as categorised Flex spending rather than held.**

Pairing is presence-based (a matching main-side row exists in this run's fetch), independent of whether that main-side row imported or was held — so an unmapped Flex account still correctly suppresses mirrors.

### 4. `TransferTarget::FlexMirror`

Add a variant to the existing `TransferTarget` enum: `FlexMirror`. It is not a destination but a marker that this row is the Flex-account side of a Flex transfer (suppress-if-paired-else-hold). Kept in the `transfer` field for uniform partitioning. `HeldTransfer` already stores `target_kind`/`target_id` strings — `FlexMirror` serialises as `target_kind = "flex_mirror"` (no id).

### 5. Direct Flex purchases — no new code

`scheme == "mastercard"` lines on the Flex feed (`is_flex_direct_virtual_card_tx`) have `transfer = None` → ordinary Phase-1/2 path → routed to the Flex Actual account (Phase-1 routing by `source_monzo_id`) and classified by Claude/cache. Confirm via test that they are NOT misclassified as transfers and NOT double-counted (main feed has no twin).

## Reuse vs new

- **Reused (already merged):** `TransferTarget::Account` resolution, transfer-payee lookup + import, `grouped` routing, `held_transfers` queue + retry, `aba accounts` suggest/persist flow, Monzo `scheme` parsing.
- **New:** exclude `monzoflexbackingloan_`; `AccountLink.is_flex` + auto-detect; `monzo_flex` → `Account(flex)` / `FlexMirror` classification (needs the flex account id at fetch time); `FlexMirror` variant; the pair-else-hold mirror suppression in `run()`; summary line for held Flex mirrors.

## Testing strategy (TDD)

Unit:
- Monzo parse: `monzo_flex` from main account → `Account(flex_id)`; `monzo_flex` from the Flex account → `FlexMirror`; `mastercard` on Flex feed → `None` (ordinary).
- Flex-account detection: `monzoflex_` description → `is_flex=true`; `monzoflexbackingloan_` excluded from `list_accounts`/suggestions; `user_` → not flex.
- Pairing: a repayment pair (main −X / flex +X same date) → main imported as transfer, flex mirror suppressed (no second import). A "flex it" pair likewise. An **unpaired** `FlexMirror` → held + counted, not dropped, not imported.

Integration (`run()`, fake-shim style):
- Repayment: exactly one transfer imported into the main account routed to the Flex transfer payee; Flex mirror produces no extra import; `held_transfers` empty.
- Direct purchase: imported as categorised spending into the Flex Actual account via the ordinary path; no transfer, no duplicate.
- Unpaired flex mirror: held, surfaced in summary, retried next run (or stays held).
- Regression: ordinary transactions and the pots path unchanged.

## Invariants honored

- **Fail-loud / never-drop:** mirrors are dropped ONLY when a main-side twin is present (zero unique data lost); unpaired/unknown Flex lines are held, not dropped. Unmapped Flex account → held + retried.
- **No double-count:** exactly one side of each Flex transfer is imported; Actual recreates the other.
- **No regression:** `scheme != monzo_flex` rows (incl. direct Flex purchases) take the unchanged Phase-1/2 path.
- Relates to [[fetch-failloud-invariant]], [[multi-account-routing-phase1]].

## Build / merge order

One slice on top of the merged pots work, sequenced:
1. Flex account identification (`is_flex` + backing-loan exclusion + `aba accounts`).
2. `monzo_flex` classification (`Account(flex)` / `FlexMirror`) in the Monzo fetch.
3. `FlexMirror` variant + pair-else-hold suppression in `run()` + summary line.
4. Direct-purchase regression test.
Green + clippy/fmt clean → merge to `main` (local-first). Validate the interest/fees OPEN ITEM against real data before/while building.
