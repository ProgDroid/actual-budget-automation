# Phase 3 — Transfers (Pots + Inter-Account) Design

**Date:** 2026-06-04
**Status:** Approved (design); implementation pending
**Phase:** 3 of the 4-phase aba enhancement (1 multi-account routing ✅, 2 categories ✅, **3 transfers**, 4 telegram grouping ⏳)
**Predecessors:** reuses the Phase-1 account map (`[[monzo.accounts]]`) and the Phase-2 import/hold plumbing.

## Goal

Turn Monzo "transfer" movements into proper **Actual transfers** (linked transactions between two Actual accounts) instead of importing them as ordinary categorised spending. Two kinds are in scope:

1. **Pot moves** — money between a Monzo current account and a savings Pot. Single-sided in the feed; high-confidence detection.
2. **Inter-account transfers** — money between two *separately mapped* Monzo accounts (e.g. personal → joint). Double-sided in the feed; needs dedup.

**Packaging decision:** one spec, **implement & merge pots first**, then inter-account on the same pipeline. The inter-account half rests on a Medium-confidence routing assumption (p2p vs Faster Payments) that is validated against the real feed before/while building that slice.

## Mechanism (verified)

Actual links transactions via **transfer payees**: a payee row with `transfer_acct` set to an account id. Importing a transaction whose `payee` is account B's transfer payee makes Actual auto-create the linked counterpart in B.

Verified against the installed `@actual-app/api`:
- The payee model has a `transfer_acct` field (`f("id", { ref: "accounts" })`), and there is a direct lookup `SELECT * FROM payees WHERE transfer_acct = ?`.
- `importTransactions` → `reconcileTransactions` → `batchUpdateTransactions({added, updated})` with `runTransfers` defaulting to **true**, so the linked counterpart **is** created on import. No switch to `addTransactions` is required.

Monzo data shape (researched against docs.monzo.com + reverse-engineered schema libs; see Research Notes):
- **Pot move:** `scheme == "uk_retail_pot"`; `metadata.pot_id` matches `GET /pots[].id`; counterparty empty; **deposit = negative** on the current account. *(Do NOT use `metadata.pot_account_id` — that is an internal id, not the `GET /pots` id.)*
- **Monzo→Monzo (p2p):** `scheme == "p2p_payment"`; `counterparty.account_id` is the destination `acc_…` **directly**; `metadata.p2p_transfer_id` is shared across both feed sides. Appears in BOTH accounts' feeds (outflow negative, inflow positive).
- **FPS/BACS caveat (Medium confidence):** some same-user moves may route as `payport_faster_payments`, exposing only `counterparty.sort_code`/`account_number` (no `acc_…`). Handled as a known gap to validate on the real feed during the inter-account slice; the p2p path is primary.

## 1. Data model

New state on `MonzoState` (mirrors `AccountLink`):
```rust
pub struct PotLink {
    pub monzo_pot_id: String,    // pot_… , matches GET /pots[].id
    pub monzo_pot_name: String,  // suggest/review UX only
    pub actual_id: String,       // Actual account UUID; "" = unmapped
    pub actual_name: String,     // suggest/review UX only
}
// MonzoState gains: pub pots: Vec<PotLink>   (serde default = empty)
```

`RawTransaction` gains a transfer discriminator (parsed at fetch, resolved at import):
```rust
pub enum TransferTarget {
    Pot(String),      // monzo pot_id
    Account(String),  // destination monzo acc_… (from p2p counterparty.account_id)
}
// RawTransaction gains: pub transfer: Option<TransferTarget>
//   None => ordinary txn  (Phase 1/2 path, unchanged)
//   Some => route to a transfer payee; no Claude classification
```

`MonzoTx` widens to parse `scheme`, `metadata` (`pot_id`), and `counterparty` (`account_id`). All new fields are `#[serde(default)]` / `Option` so existing parsing is unaffected.

**Classification rule (at fetch):**
- `scheme == "uk_retail_pot"` → `TransferTarget::Pot(metadata.pot_id)`.
- `scheme == "p2p_payment"` AND `counterparty.account_id ∈ {our mapped monzo_id}` → `TransferTarget::Account(counterparty.account_id)`.
- otherwise → `transfer = None` (ordinary). A p2p payment to an account we do **not** map is an ordinary txn (it's an external person, not a tracked account).

## 2. Monzo fetch changes

- `fetch_account` (already per-account) parses the new `MonzoTx` fields and sets `RawTransaction.transfer`. Detection is **self-contained in transaction metadata** — no extra call is required for detection.
- A per-account `GET /pots?current_account_id=…` is added **only to populate pot names** for the mapping command (`aba accounts`), not for detection.
- Pot/transfer txns are NOT removed by the decline filter; they flow through with `transfer: Some(...)`, sign preserved (deposit negative).
- Fail-loud unchanged: any fetch error still aborts the whole run (no partial Ok); `last_run` only advances on success. See [[fetch-failloud-invariant]].

## 3. Import resolution (transfer-payee path)

- **Shim:** `get_payees` extended to return `transfer_acct` (extra field; `null` for normal payees). aba builds `actual_account_id → transfer_payee_id`.
- In `run`, a `RawTransaction` with `transfer: Some(target)`:
  1. Resolve `target` → destination `actual_id` (`PotLink` for `Pot`, `AccountLink` for `Account`).
  2. Look up that account's transfer payee id.
  3. Emit `ImportTxn { payee: transfer_payee_id, category: None, … }`, grouped into the **source** account (same routing as today).
  4. `importTransactions` auto-creates the linked counterpart in the destination.
- Transfers **bypass Claude** entirely (no payee/category classification) and import **uncategorised** (`category = None`) — the standard Actual treatment for transfers.

## 4. Inter-account dedup (double-counting fix)

A Monzo→Monzo p2p move appears in both feeds. **Rule: import the outflow (negative) side as the transfer; suppress the inflow (positive) side** whose `counterparty.account_id` is one of our mapped accounts (Actual creates that inflow itself).

- Suppression occurs at candidate assembly in `run`, identifying the inflow counterpart of an internal transfer (positive amount, `transfer == Account(ours)`). `metadata.p2p_transfer_id` may be used to pair sides if needed.
- Pots are exempt (single-sided feed — no suppression).
- Idempotency unchanged: the surviving outflow keeps its Monzo tx id as `imported_id`; the suppressed inflow is never imported.

## 5. Held transfers — durable retry queue

A transfer whose destination is **unmapped** is held (fail-loud — never imported into the wrong place; never dropped). This is **not** a payee/category question, so it does **not** use `pending_reviews`/`build_prompt`/Telegram inline prompts. It uses a dedicated durable queue with **"retry when mapped"** semantics:

```rust
pub struct HeldTransfer {
    pub external_id: String,      // Monzo tx id (idempotency / dedup key)
    pub source_monzo_id: String,
    pub date: String,
    pub amount: i64,
    pub target: TransferTarget,   // Pot(id) | Account(acc_…)
    pub raw_merchant: String,     // notification text
}
// State gains: pub held_transfers: Vec<HeldTransfer>   (serde default = empty)
```

**Per-run lifecycle:**
1. Before/with assembling imports, re-attempt to resolve each `held_transfer.target → actual_id`. If now mapped → emit the transfer `ImportTxn` and remove from the queue. If still unmapped → keep.
2. Newly fetched transfers with an unmapped destination are appended (deduped by `external_id`).
3. Because held transfers live in `State` and are retried indefinitely, advancing `last_run` past them does not lose them ([[fetch-failloud-invariant]]).

**Surfacing:** the run summary/notification gains a separate line, e.g. `⤢ 3 transfer(s) awaiting account mapping: 2→'Savings', 1→acc_… — run \`aba accounts\` to map.` Counted separately from the interactive `held` count. No Telegram prompt.

## 6. Mapping UX — extend `aba accounts`

Reuse the existing suggest→write flow rather than adding a new command:
- `aba accounts` lists Monzo accounts (today) **and** pots (per current account, via `GET /pots`), suggesting Actual-account matches by name similarity for both.
- `aba accounts --write` persists both `[[monzo.accounts]]` and `[[monzo.pots]]`.
- The user creates the Actual accounts (on/off-budget is their choice — aba only needs `actual_id`) and edits the TOML to confirm/override.
- Pots the user doesn't care to track are left unmapped → their moves sit in the retry queue (accepted trade-off of the fail-loud choice).
- RUNBOOK updated with the pots mapping step.

## 7. Testing strategy (TDD)

Unit/integration (mock bank + mock Actual, existing patterns):
- **Parse:** `uk_retail_pot` → `Pot`; `p2p_payment` to our acc → `Account`; p2p to a non-mapped acc → ordinary (not a transfer); sign preserved.
- **Resolution:** mapped pot → `ImportTxn { payee = transfer_payee, category = None }` grouped into source account; bypasses Claude.
- **Dedup:** an A→B pair yields exactly one outflow import; the B inflow is suppressed.
- **Held queue:** unmapped → queued + counted (not prompted); **retry imports once mapped**; idempotent across runs (no duplicate queue entries, no double import).
- **Degradation:** `transfer == None` path is byte-for-byte the Phase 1/2 behavior (zero change for users with no pots/transfers).

E2e (`run.rs` style): full pots happy-path + a held-then-mapped cycle. Shim test: `get_payees` returns `transfer_acct`.

## Build / merge order

1. **Pots slice** — sections 1–3, 5, 6, 7 (minus inter-account). Green + clippy/`cargo fmt` clean → merge to `main` (local-first; push is a separate explicit step).
2. **Inter-account slice** — section 4 + its tests, on top. Validate p2p-vs-FPS routing against the real feed first.

## Invariants honored

- **Fail-loud, never drop** ([[fetch-failloud-invariant]]): unmapped transfers held in a durable retry queue, not silently imported or lost.
- **Idempotency:** Monzo tx id remains the `imported_id`; suppressed sides never imported.
- **No regression:** ordinary transactions (`transfer == None`) take the unchanged Phase 1/2 path.
- Relates to [[monzo-token-rotation-invariant]], [[rust-harness-gotchas]].

## Research Notes (Monzo API, 2026-06-04)

Confidence per claim from cross-verified sources (docs.monzo.com, `monzo/docs` GitHub, `trickeydan/monzo-webhook-schema`, `lilopkins/monzo-webhook-rs`, billinghamj payload gist):
- **High:** `uk_retail_pot` scheme; `metadata.pot_id` ↔ `GET /pots[].id`; deposit negative; `p2p_payment` exposes `counterparty.account_id`; both sides appear in both feeds; pot object fields (`id,name,style,balance,currency,created,updated,deleted`).
- **Medium:** `p2p_transfer_id` byte-identical across both feed sides; exactly when same-user moves route p2p vs FPS. **Both are load-bearing only for the inter-account slice and are validated against the real feed before building it.**
- The public docs omit `scheme`/`counterparty`/rich `metadata`; those field names come from observed payloads, not prose docs.
