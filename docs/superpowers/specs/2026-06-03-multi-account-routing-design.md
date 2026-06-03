# Multi-Account Routing & the `account_id` Split — Design (Phase 1)

Date: 2026-06-03
Status: Approved (brainstorming) — pending written-spec review

## Background

This is **Phase 1 of four** agreed features. The full set, in build order:

1. **`account_id` split + multi-account routing** ← *this spec*
2. Categories (Claude-classified `category_id` and/or Actual rules)
3. Transfers (incl. Monzo pots, which are transfers in the parent feed)
4. Telegram prompts grouped per merchant

Each phase ships independently. Phase 3 reuses the account map built here.

## Problem

Two distinct problems, one root cause.

`state.monzo.account_id` is **overloaded** — the same string is used as two
different identifiers in two different namespaces:

- `src/banks/monzo.rs::fetch_since` uses it as the **Monzo** account id
  (`acc_…`) in `GET /transactions?account_id=`.
- `src/run.rs::run` passes it straight to
  `actual.import_transactions(state.monzo.account_id, …)` as the **Actual**
  account id (a UUID).

A Monzo `acc_…` id is never a valid Actual account UUID, so `importTransactions`
accepts the rows but cannot attach them to a real account → **"transactions
added without an account."** This is the bug the user observed.

Separately, the user has **two real Monzo accounts** (personal + joint) plus
**several pots**, all mirrored as accounts in Actual. Today only one Monzo
account is fetched, and everything imports to the single (mis-used) id. They
want each Monzo account's transactions routed to its corresponding Actual
account.

## Scope

In scope (Phase 1):

- Split the overloaded id into a per-account **Monzo→Actual link**.
- Fetch transactions from **each** configured Monzo account.
- Route each transaction to the **correct Actual account** on import.
- A discovery command that **auto-suggests** the mapping (matched by name),
  which the user reviews and persists ("manual + auto-suggest").

Out of scope (later phases), called out so the boundaries are explicit:

- **Pots.** Monzo pots have no independent `/transactions` feed; pot
  deposits/withdrawals appear *inside the parent account's* feed as entries
  carrying `metadata.pot_id`. Mirroring a pot as its own Actual account means
  turning each pot movement into a **transfer** between the parent Actual
  account and the pot Actual account — that is Phase 3. Phase 1 routes only the
  two real accounts. The discovery command *lists* pots for the user's
  awareness but does not link them.
- Categories (Phase 2), transfers (Phase 3), Telegram grouping (Phase 4).

## Approach (chosen: "A")

State-held account links plus an interactive discovery command. Rejected
alternatives: a minimal two-singular-ids fix (no auto-suggest, doesn't scale to
pots) and a config-file map (no auto-suggest, mixes discovered ids into
secrets-style config). Approach A is the only one delivering the chosen
"manual + auto-suggest" and it produces the account map Phase 3 reuses.

## Data model changes

### `MonzoState` (src/state.rs)

Replace the single `account_id` with a list of links, keeping the old field for
backward-compatible deserialisation and migration.

```rust
#[derive(Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonzoState {
    #[serde(default)]
    pub refresh_token: String,

    /// Legacy single Monzo account id. Retained ONLY so old state files still
    /// deserialise; migrated into `accounts` on load (see State::load). New
    /// code must not read this directly.
    #[serde(default)]
    pub account_id: String,

    /// Monzo→Actual account links. Empty until `aba accounts` populates it.
    #[serde(default)]
    pub accounts: Vec<AccountLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountLink {
    /// Monzo account id (`acc_…`) — used for fetching.
    pub monzo_id: String,
    /// Monzo account display name (for the suggest/review UX only).
    #[serde(default)]
    pub monzo_name: String,
    /// Actual account UUID — used as the import target. Empty = unmapped.
    #[serde(default)]
    pub actual_id: String,
    /// Actual account name (for the review UX only).
    #[serde(default)]
    pub actual_name: String,
}
```

**Migration (in `State::load`, or a normalise step after load):** if `accounts`
is empty and the legacy `account_id` is non-empty, push a single
`AccountLink { monzo_id: account_id, actual_id: "" , .. }` and clear the legacy
field. `actual_id` is intentionally left empty because the legacy value was the
*Monzo* id — the user must run `aba accounts` to supply the Actual side. An
unmapped link (empty `actual_id`) is handled fail-loud at import time (below).

### `RawTransaction` (src/banks/mod.rs)

Add the source account so `run.rs` can route it:

```rust
pub struct RawTransaction {
    pub external_id: String,
    pub date: NaiveDate,
    pub amount_minor: i64,
    pub raw_merchant: String,
    /// Monzo account id this transaction was fetched from. Routes the import.
    pub source_monzo_id: String,
}
```

`external_id` (Monzo's globally-unique tx id) remains the `imported_id`, so
idempotency is unchanged and unaffected by routing.

### Shim protocol (src/actual/shim_protocol.rs + actual-shim/index.js)

Add a read-only op to list Actual accounts (needed by `aba accounts`):

```rust
// ShimRequest
GetAccounts,

// new response struct
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Account { pub id: String, pub name: String }
```

```js
// actual-shim/index.js, in handle()
case 'get_accounts': {
  const accounts = await api.getAccounts();
  return accounts.map((a) => ({ id: a.id, name: a.name }));
}
```

`ActualClient::get_accounts()` mirrors the existing `get_payees()`.

## Component changes

### `src/banks/monzo.rs` — fetch per account

- `Monzo` iterates `state.monzo.accounts`, fetching each `monzo_id` with the
  existing paginated cursor walk, and tags every produced `RawTransaction` with
  `source_monzo_id`.
- A link with an empty `monzo_id` is skipped with a warning (shouldn't happen).
- New helper for discovery: `list_accounts(&self) -> Result<Vec<MonzoAccount>>`
  hitting `GET /accounts` (and a `list_pots` hitting `GET /pots?current_account_id=…`
  for the *informational* pot listing). These require a valid access token, so
  `aba accounts` calls `ensure_auth` first, exactly like `run`.
- The "no accounts configured" guard changes from checking a single
  `account_id` to: error fail-loud if `state.monzo.accounts` is empty, telling
  the user to run `aba accounts`.

Fetch remains **fail-loud** (existing invariant): if any account's fetch errors,
the whole run returns `Err` and `last_run` is not advanced — no partial-Ok that
would silently drop the other account's data.

### `src/run.rs` — route on import

The matching pipeline (schedule drop, cache split, Claude, pending reviews) is
**account-agnostic** and unchanged — payee resolution does not depend on which
account a transaction came from.

Only assembly + import change:

- `ImportTxn` gains no field; instead `run` builds a
  `BTreeMap<&str /*actual_id*/, Vec<ImportTxn>>` keyed by the source
  transaction's `actual_id` (resolved via `source_monzo_id` → `AccountLink`).
- A transaction whose source account is **unmapped** (`actual_id` empty) or
  unknown is **held fail-loud**, not imported: log a warning and surface it in
  the run summary so it's visible, never silently dropped. (Re-uses the
  existing pending/summary machinery; these are config gaps, not review items —
  exact surfacing decided in the plan, but the invariant is "never silently
  dropped".)
- Import once per Actual account:
  `for (actual_id, rows) in grouped { actual.import_transactions(actual_id, rows).await? }`,
  accumulating `added`/`updated` into the existing `ImportSummary` for the
  notification.

`last_run`/token-persist ordering and the dry-run path are preserved; dry-run
prints the per-account grouping.

### New CLI: `aba accounts` (src/config.rs, src/main.rs, new flow)

Consistent with `aba auth monzo` (run where it has the needed access; output
reviewed before it lands in server state).

```
aba accounts          # fetch both sides, print the auto-suggested mapping, NO write
aba accounts --write  # same, then persist the mapping into state.monzo.accounts
```

Flow:

1. `ensure_auth` (Monzo) + persist rotated token immediately (existing invariant).
2. `monzo.list_accounts()` and `monzo.list_pots()`.
3. `actual.get_accounts()`.
4. **Auto-suggest**: for each Monzo account, pick the best Actual account by
   normalised-name similarity, reusing `matching::normalise` /
   `matching::payee_similarity` (same machinery already trusted for payees).
   Above a threshold → suggested link; below → left unmapped with a note.
5. Print a review table: `monzo_name (monzo_id) → actual_name (actual_id)
   [confidence]`, plus a list of pots flagged "Phase 3 — not linked yet."
6. On `--write`, persist the suggested links to `state.monzo.accounts` and save.

**Override model:** auto-suggest fills the mapping; the user overrides a wrong
suggestion by editing the `accounts` array in the state file (small, explicit,
works in the one-shot container model). Re-running `aba accounts` (without
`--write`) re-prints suggestions for comparison. This satisfies "manual +
auto-suggest" without fragile interactive stdin prompts inside a one-shot
container.

## Data flow (after Phase 1)

```
aba accounts --write
  Monzo /accounts ─┐
  Monzo /pots ─────┤ auto-suggest by name ─→ state.monzo.accounts (review/override)
  Actual getAccounts ┘

aba run
  for link in accounts: Monzo /transactions?account_id=link.monzo_id
    → RawTransaction{ …, source_monzo_id }
  → (schedule drop, cache/Claude payee match — unchanged)
  → group ImportTxn by link.actual_id
  → for (actual_id, rows): api.importTransactions(actual_id, rows)
```

## Error handling

- **Unmapped/unknown source account** → held fail-loud + surfaced in summary;
  never silently dropped (consistent with the fetch fail-loud invariant).
- **Empty `accounts`** → `run` errors with guidance to run `aba accounts`;
  `last_run` not advanced.
- **`get_accounts` shim failure** → `aba accounts` errors; nothing written.
- **Monzo `/accounts` or `/pots` failure** → `aba accounts` errors; auth token
  already persisted before the fallible calls (existing ordering invariant).
- Per-account import error → whole `run` returns `Err`; already-imported
  accounts are idempotent on retry via `imported_id`, so a retry is safe.

## Testing

- **State migration**: legacy `{account_id}` state deserialises into one
  unmapped `AccountLink`; new `{accounts:[…]}` round-trips.
- **Routing**: given two links and a mix of `source_monzo_id`s, `run` produces
  one `importTransactions` call per `actual_id` with the right rows; unmapped
  source → held, not imported (assert summary/held count).
- **Fetch tagging**: `fetch_since` over two mocked accounts tags each
  `RawTransaction.source_monzo_id` correctly and remains fail-loud if one
  account errors.
- **Auto-suggest**: name-similarity picks the expected Actual account above
  threshold and leaves genuinely-unmatched Monzo accounts unmapped.
- **Shim** `get_accounts`: protocol (de)serialisation round-trip; thin
  JS op covered by existing shim test pattern.
- Existing `run`/matching/monzo tests updated for the new `RawTransaction`
  field and the `accounts` state shape.

## Open items for the implementation plan

- Exact surfacing of "unmapped source account" in the run summary / notifier.
- Name-similarity threshold for auto-suggest (start from the payee threshold,
  tune).
- Whether `list_pots` lands in Phase 1 (informational only) or is deferred with
  pots entirely — leaning: include the listing, it's cheap and orients the user
  for Phase 3.
