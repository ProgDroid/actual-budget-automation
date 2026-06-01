# aba — Phase 2 Backlog

Deferred work captured during Phase 1 (2026-06-01). Each item should get its own
brainstorm → spec → plan cycle before implementation. Ordered roughly by value.

## 1. Interactive payee-match confirmation loop (Telegram two-way)

**Motivation.** In Phase 1, confident Claude payee matches are auto-imported and
cached (Option B), with a one-way Apprise summary of what was newly auto-matched.
A wrong match is sticky until the user edits the state file. We want an optional
*confirm/correct* step so a proposed match can be approved or corrected from the
phone before it becomes permanent.

**Why it's a real feature, not a bolt-on.**
- Apprise is **outbound only**. Reading replies needs a Telegram-specific
  integration (Bot API `getUpdates` or a webhook) — a new dependency.
- The binary is a **one-shot nightly cron job**; it cannot block waiting for a
  human reply. The loop must be **asynchronous**.

**Proposed shape (fits the one-shot model).**
- Run N: a new merchant is matched by Claude → instead of auto-importing/caching,
  it is **held as a pre-filled pending review** (Claude's suggestion populated in
  `resolve_to`) and a Telegram message is sent: "Map `TFL TRAVEL CH` →
  `Transport for London`? Reply ✅ to confirm, or send the correct payee name."
- The user replies whenever.
- Run N+1: a **poll step** (`getUpdates`) reads replies, applies them to the state
  file (cache + mark resolved), then proceeds with the day's import.
- Equivalent to extending the existing **pending-review mechanism** with: (a) a
  pre-filled suggestion, and (b) an inbound reply channel.

**Design questions to resolve in the Phase 2 brainstorm.**
- Confirm *all* new matches, or only *low-confidence* ones? (Requires Claude to
  return a confidence signal + a threshold — a "confidence-gated hybrid".)
- Reply correlation: how to map a Telegram reply back to a specific pending entry
  (inline keyboard callback data vs. free-text parsing).
- Polling cadence / offset bookkeeping for `getUpdates` (store last update_id in
  state).
- Fallback when Telegram is unreachable (don't block the import).
- Keep Apprise for one-way alerts; add Telegram only for the confirm channel.

## 2. GoCardless / Nordigen bank source (Santander + other UK banks)

Already planned as Phase 2 in the original spec. Implement the existing
`BankSource` trait for GoCardless/Nordigen. Reuse the same fetch → match → import
pipeline; only the auth + fetch + transaction mapping differ.

## 3. Monzo transaction pagination

`fetch_since` currently requests `limit=100` and logs a warning if exactly 100 are
returned, but does **not** paginate. Implement cursor-based pagination (Monzo
`before` parameter) so high-volume windows aren't silently truncated. Today the
warning is the only safeguard.

## 4. `run()` integration test coverage

`run()` has no automated test (only its pure helpers do). Highest-risk untested
paths to cover before adding more write paths:
- **Resolved-review + `create_payee` transactional hazard**: payee is created
  before the import call; a failure between the two leaves a created payee with no
  attached transaction and no clean retry.
- **Dry-run state-splice correctness**: assert that after a dry run, `payee_cache`,
  `pending_reviews`, and `last_run` are unchanged on disk and only the rotated
  Monzo token is persisted.
- **`AuthExpired` early-return**: notification fires only when `!dry_run`; `save`
  is always attempted.

Requires test doubles for `BankSource`, `ClaudeMatcher`, `Notifier`, and a fake
`node` shim (as used in `actual::tests`).

## 5. Minor hardening (from Phase 1 review)

- **OAuth `state` not verified** (`auth_monzo`): the `state` param is generated but
  not checked against the redirect. Low risk (localhost, single-use, developer
  only) but worth verifying before any multi-user/hosted deployment.
- **Variable-amount schedules degrade to `0`** (`actual-shim/index.js`): when a
  schedule's `_amount` is a recurrence object without `num1`, the row carries
  `amount: 0`, which never matches a real transaction (false negative → the
  scheduled txn is imported instead of skipped). Handle ranged/variable schedule
  amounts explicitly.
- **Dry-run "would hold" count** can overstate vs. the live `held` count when
  pending entries already exist (cosmetic).

## 6. Operational notes (not code, but easy to forget)

- The runtime container runs as the non-root `node` user (uid 1000). The mounted
  `/data` volume (state file + `ACTUAL_DATA_DIR`) must be **writable by uid 1000**
  on the host, or the container can't persist state. Document/automate the
  `chown 1000:1000 /srv/aba` (or equivalent) step.
- Prompt caching does **not** help at the once-daily cadence (≈5-min ephemeral TTL
  expires between runs). It only pays off for runs <5 min apart (retries/testing)
  or a higher-frequency schedule. Revisit if the schedule changes.
