# Actual Budget Automation — Design

**Date:** 2026-06-01
**Status:** Approved for planning

## Overview

A Rust binary (`aba`) that fetches bank transactions daily, normalises payee
names via the Claude API, and imports them into a self-hosted Actual Budget
instance. A thin Node.js shim wraps `@actual-app/api` and is driven by the Rust
binary over stdin/stdout JSON. The binary ships as a Docker image to GHCR and
runs **one-shot** on the user's server, on the same Docker network as Actual.

Bank sources are pluggable behind a trait. **Monzo** (first-party OAuth2) is
implemented first; **GoCardless/Nordigen** (for Santander and other UK accounts)
is phase 2, implemented against the same trait once Monzo works end to end.

## Goals

- Hands-off daily import of bank transactions into Actual.
- Normalise raw merchant names to existing Actual payees using Claude.
- **Never** silently auto-create payees: genuinely new payees are held for
  manual review.
- **Never** silently fail: every notable condition is reported via Apprise.
- Safe to test against a real budget via a `--dry-run` mode that performs no
  writes to Actual.

## Non-goals (this phase)

- GoCardless/Nordigen implementation (trait is designed now, implemented later).
- An interactive `review` subcommand (resolution is done by editing the state
  file).
- Any web UI or long-running daemon.

## Architecture & runtime

- **Single Rust binary** `aba`, distributed as a Docker image published to GHCR.
- On the server it runs **one-shot**:
  `docker run --rm --network <actual-net> ghcr.io/<owner>/aba:latest run`,
  triggered by **host cron or a systemd timer**. Zero idle memory between runs
  (the primary server constraint); CPU is a brief once-daily spike.
- Communicates with Actual over **plain HTTP on the shared Docker network**
  (e.g. `http://actual:5006`), avoiding all TLS/cert concerns.
- A small **Node.js shim** (`actual-shim/`, npm) wraps `@actual-app/api`. The
  Rust binary spawns a short-lived Node process per operation and exchanges
  **JSON over stdin/stdout**.
- **State** persists in a **TOML file on a mounted host volume** (required so
  state survives one-shot container death; also makes manual review edits a
  simple file edit on the host).

### Rationale: one-shot vs. resident scheduler

Memory is the main server constraint. A resident scheduler holds RAM 24/7; a
one-shot container uses **0 MB** between runs and only consumes memory during the
~30–60 s run. The cost is one host cron/timer line, which also forces the
state-on-mounted-volume property we want anyway.

## Module layout (single crate)

```
src/
  main.rs            # CLI dispatch: `run` (+ --dry-run), `auth monzo`
  config.rs          # env + CLI parsing
  state.rs           # load/save TOML: last_run, payee_cache, pending_reviews, monzo tokens
  banks/
    mod.rs           # `BankSource` trait
    monzo.rs         # OAuth2 + /transactions
    gocardless.rs    # phase 2, same trait
  actual/
    mod.rs           # Rust-side client: spawns shim, serializes ops, --read-only passthrough
    shim_protocol.rs # request/response JSON types
  claude.rs          # batch payee-matching via Anthropic API (+ prompt caching)
  matching.rs        # scheduled-txn fuzzy skip + payee-cache lookup
  notify.rs          # Apprise wrapper
actual-shim/
  index.js
  package.json
  package-lock.json
```

### The `BankSource` trait

A bank source can ensure a valid access token (refreshing as needed) and fetch
transactions since a timestamp. Sketch:

```rust
pub struct RawTransaction {
    pub external_id: String,      // bank's stable txn id -> Actual imported_id
    pub date: NaiveDate,
    pub amount_minor: i64,        // integer minor units (pence), sign per Actual convention
    pub raw_merchant: String,     // merchant.name, fallback to description
}

pub trait BankSource {
    fn name(&self) -> &'static str;
    /// Ensure a usable access token; refresh if needed. On unrecoverable
    /// auth expiry, return AuthExpired so the caller can notify + skip.
    fn ensure_auth(&mut self, state: &mut State) -> Result<(), BankError>;
    fn fetch_since(&self, since: DateTime<Utc>) -> Result<Vec<RawTransaction>, BankError>;
}
```

`BankError::AuthExpired` is handled specially (notify + skip the bank, continue
the rest of the run, exit 0).

## Core flow (`aba run`)

1. Load state. For each enabled bank, `ensure_auth`. On `AuthExpired`: send a
   plain Apprise alert, skip that bank, continue.
2. `fetch_since(last_run)` per bank.
3. Via shim: get Actual payees and scheduled transactions.
4. **Scheduled skip:** drop any transaction that matches a scheduled entry —
   fuzzy match on *exact amount* + *date within ±N days* + *fuzzy payee name*.
   (N configurable, default ±3 days.)
5. **Payee resolution**, in order:
   1. **Payee cache** hit (raw merchant → payee) — use directly, no Claude call.
   2. **Claude batch match** remaining raw merchants against the existing Actual
      payee list. The payee list is sent as a **prompt-cache breakpoint** so
      repeated daily runs pay full input price once, then ~10% thereafter.
   3. **Unmatched** ⇒ held as a `pending_review` entry. Not imported, not
      created.
6. **Import** matched transactions via the shim, using the bank's `external_id`
   as Actual's **`imported_id`** so Actual dedupes idempotently. Amounts are
   **integer minor units**.
7. Persist rotated bank tokens. Advance `last_run` only to cover successfully
   processed transactions. Append new pending reviews. Send a **summary** Apprise
   notification (imported N, held M, skipped K).

### Dry-run mode (`aba run --dry-run`)

Runs the full pipeline (fetch → scheduled-skip → cache/Claude match) but performs
**no writes to the Actual budget**:

- The Rust side never calls a mutating shim op; the shim is additionally launched
  with `--read-only` as defence in depth (it refuses mutations).
- `last_run` is **not** advanced; `payee_cache` and `pending_reviews` are **not**
  modified; **no** Apprise notification is sent.
- **Exception (unavoidable):** if fetching required a Monzo token refresh, the
  rotated refresh token **is** persisted — discarding it would break the next
  real run, since Monzo rotates refresh tokens on every refresh.
- Output: a printed report — **would-import** (txn → resolved payee),
  **would-skip** (matched scheduled entry), **would-hold** (new payee) — plus a
  summary line.

## Manual review resolution (state file + payee cache)

When Claude finds no match, the transaction is **held** (never auto-imported,
never auto-created) and written as a `pending_review` entry. The summary Apprise
alert lists held payees. To resolve, the user edits the mounted state file:

- set `resolve_to = "<existing payee name>"`, **or**
- set `create_new = true` to create a new payee in Actual.

On the next run, each resolved `pending_review` is applied (the held transaction
is imported, the payee created if requested) and the raw→payee answer is folded
into `payee_cache`, so that raw merchant is **never re-prompted** again. Resolved
entries are then removed from `pending_review`.

## State file schema (TOML)

On the mounted volume; **gitignored**; contains the rotating Monzo refresh token.

```toml
last_run = "2026-05-31T03:00:00Z"

[monzo]
refresh_token = "..."          # rotated on every refresh; server persists each run
account_id    = "acc_..."

[payee_cache]                  # raw merchant -> resolved payee, built from resolutions
"TFL TRAVEL CH" = "Transport for London"

[[pending_review]]             # user edits these; resolved ones fold into payee_cache
raw_merchant = "SQ *SOME CAFE"
amount = -450
date = "2026-05-30"
resolve_to = ""                # set to an existing payee name…
create_new = false             # …or true to create it in Actual
```

## Authentication

### Monzo OAuth2

- **Tokens:** short-lived access token + refresh token. The server refreshes the
  access token as needed and **persists the rotated refresh token every run**.
- **90-day re-auth:** Monzo requires full re-authentication every 90 days; refresh
  stops working. This needs a browser and is handled out-of-band (below).
- **SCA:** immediately after the first token exchange, the user must tap
  "Approve" in the Monzo mobile app, or the API only returns transactions from
  the last 5 minutes. The bootstrap command pauses and instructs the user.
- **Merchant name:** `merchant.name`, falling back to `description`.

### Bootstrap & re-auth (headless server)

The server is headless (web UIs only), so the OAuth browser flow runs on the
user's **dev machine**, not the server:

1. `aba auth monzo` (run on the dev machine) constructs the OAuth URL locally,
   opens the browser, listens on `MONZO_REDIRECT_URI` (e.g.
   `http://localhost:8080/callback`), exchanges the code, and **prints the
   refresh token** plus a ready-to-paste `refresh_token = "..."` line.
2. The user places that token into the server's state file on the mounted volume
   (editor or `scp`).
3. Subsequent server runs self-refresh until the 90-day expiry.

**Re-auth loop:** when a daily run's refresh fails, the server sends a **plain**
Apprise alert ("Monzo re-auth required — run `aba auth monzo` and update the
server token"), skips Monzo, and exits 0. The notification carries **no OAuth
link** — the link is generated locally by the command. The user re-runs the
command and updates the server token.

## Error handling & notifications (Apprise)

- **Monzo auth expiry:** plain alert, skip the bank, continue, exit 0.
- **New unmatched payees:** summary alert listing them; held as pending; never
  blocks other imports.
- **Hard failures** (Actual unreachable, shim crash, Anthropic error): alert with
  detail; **non-zero exit** so cron/systemd surfaces the failure.

## Claude integration

- Model: **`claude-sonnet-4-6`**.
- One **batch** call per run: given the list of raw merchant names needing
  resolution and the list of existing Actual payees, return best matches (or
  "no match") per merchant.
- The existing-payee list is placed behind a **prompt-cache breakpoint** (stable
  run-to-run) to minimise per-run cost.
- Calls sit behind a trait so they can be stubbed in tests.

## Configuration / secrets

Static secrets and config via **environment variables** (nothing hardcoded):

- `ANTHROPIC_API_KEY`
- `MONZO_CLIENT_ID`, `MONZO_CLIENT_SECRET`, `MONZO_REDIRECT_URI`
- `APPRISE_URL`
- `ACTUAL_SERVER_URL`, `ACTUAL_PASSWORD`, `ACTUAL_SYNC_ID` (budget/sync id)
- `ABA_STATE_PATH` (path to the TOML state file on the mounted volume)

The rotating Monzo refresh token lives in the **state file** (operational state),
not in env (static secrets) — different lifecycles, different homes.

## Node shim protocol

The shim exposes exactly the operations needed, selected by a JSON request on
stdin and answered with a JSON response on stdout:

- `get_payees` → `[{ id, name }]`
- `get_scheduled` → scheduled transactions (amount, date/rule, payee)
- `import_transactions` → `{ account_id, transactions: [{ date, amount, payee, imported_id }] }`
- `create_payee` → `{ name } -> { id }` (only used when resolving `create_new`)

Launched with `--read-only` in dry-run mode, in which case any mutating request
is refused.

## Testing approach (TDD)

Build vertically, pure logic first:

1. **`matching.rs`** — table-driven tests for scheduled-skip fuzzy matching and
   payee-cache lookup.
2. **`state.rs`** — round-trip TOML; the pending-review → cache state machine.
3. **Monzo** — `wiremock` for the OAuth + `/transactions` HTTP surface, including
   token-rotation and `AuthExpired`.
4. **Shim** — a fake Node script (or trait double) to assert the JSON protocol
   and that dry-run/`--read-only` never mutates.
5. **Claude** — behind a trait, stubbed; assert request shape and response
   handling (matched / no-match / partial).
6. **`run` wiring** — integration test composing the above doubles, asserting the
   dry-run guarantees (no mutations except token persistence).

## Open items deferred by decision

- GoCardless/Nordigen: phase 2.
- Interactive `review` subcommand: dropped in favour of state-file editing.
