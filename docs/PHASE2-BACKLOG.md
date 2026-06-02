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

## 2. GoCardless / Nordigen bank source — SKIPPED (2026-06-02)

**Won't do for now (decision 2026-06-02).** GoCardless/Nordigen is blocked anyway
(not accepting new registrations), and the other UK accounts are much smaller, so
the user will **enter those transactions manually in Actual** rather than automate
them. Monzo (the high-volume account) stays the only automated source.

If revisited: implement the existing `BankSource` trait for the chosen aggregator
and reuse the same fetch → match → import pipeline (only auth + fetch + transaction
mapping differ). The `encode_callback` 64-byte path and stale-prompt cleanup that
this was once gated on are already handled (see #7a/#7b).

## 3. Monzo transaction pagination — DONE (2026-06-02)

**Implemented** on `main` (merge `95590b7`; spec/plan at
`docs/superpowers/specs|plans/2026-06-02-monzo-pagination-*.md`). `fetch_since`
now walks every page using Monzo's **last-transaction-id cursor** (`since=<last
raw id>`), not the `before` timestamp the original note assumed — the id cursor
avoids the same-`created`-timestamp skip/duplicate hazard. The page-size check and
the cursor both use the **raw** response count/id (before decline filtering). A
`MAX_PAGES = 100` (~10k txns) runaway guard returns `BankError::Other` and aborts
loudly rather than silently truncating (so `run.rs` never advances `last_run` on a
partial fetch). The old `warn!`-at-100 is gone.

## 4. `run()` integration test coverage — DONE (2026-06-02)

**Implemented** on `main` (spec/plan at
`docs/superpowers/specs|plans/2026-06-02-run-coverage-*.md`). Added 4 `run()`
integration tests + doubles (`RecordingNotifier`, `AuthExpiredBank`,
`RotatingBank`, `fake_actual_failing_import`): AuthExpired early-return (notify+save
gated on `!dry_run`), dry-run state-splice (only the rotated Monzo token persisted),
and create_payee→import-failure recoverability (review survives on disk → clean
retry; the hazard self-heals and is NOT a "no clean retry" bug). Original notes
retained below for reference.

`run()` had no automated test (only its pure helpers did). Highest-risk untested
paths to cover before adding more write paths:
- **Resolved-review + `create_payee` transactional hazard**: payee is created
  before the import call; a failure between the two leaves a created payee with no
  attached transaction and no clean retry.
- **Dry-run state-splice correctness**: assert that after a dry run, `payee_cache`,
  `pending_reviews`, and `last_run` are unchanged on disk and only the rotated
  Monzo token is persisted.
- **`AuthExpired` early-return**: notification **and** `save` both fire only when
  `!dry_run`; the run then early-returns `Ok` without fetching or importing.

Requires test doubles for `BankSource`, `ClaudeMatcher`, `Notifier`, and a fake
`node` shim (as used in `actual::tests`).

## 5. Minor hardening (from Phase 1 review) — DONE (2026-06-02)

**Implemented** on `main` (spec/plan at
`docs/superpowers/specs|plans/2026-06-02-phase2-hardening-*.md`). All three:

- **OAuth `state` verified** (`auth_monzo`): now a random 128-bit `getrandom`
  nonce (hex), verified against the redirect via a pure `verify_state` helper
  before the token exchange; mismatch/missing aborts with a browser failure msg.
- **Variable-amount schedules fixed**: shim emits `amount: null` for non-numeric
  `_amount`; `ScheduleRow.amount`/`Schedule.amount` are `Option<i64>`;
  `matches_scheduled` skips a `None`-amount schedule by **date + payee only**
  (so its manifestation is correctly skipped, not imported).
- **Dry-run "would hold" count fixed**: the dry-run block now prints the existing
  `held` counter (which already counts only newly-held, deduped entries) instead
  of a redundant helper. (A first attempt added a `would_hold_count` helper that
  read post-mutation state and always printed `0`; review caught it, fix reuses
  `held`.)

## 6. Operational notes (not code, but easy to forget) — DONE (2026-06-02)

**Documented** in `docs/RUNBOOK.md` (spec/plan at
`docs/superpowers/specs|plans/2026-06-02-backlog-6-7.md`). Both notes are now in
the runbook: the uid-1000 volume-ownership step (`chown -R 1000:1000 /srv/aba`)
sits at the top of the "Daily run" section, and the prompt-caching cadence
caveat is a new "Operational notes" subsection. Docs-only by decision (no
`chown` automation script). Original notes retained below.

- The runtime container runs as the non-root `node` user (uid 1000). The mounted
  `/data` volume (state file + `ACTUAL_DATA_DIR`) must be **writable by uid 1000**
  on the host, or the container can't persist state. Document/automate the
  `chown 1000:1000 /srv/aba` (or equivalent) step.
- Prompt caching does **not** help at the once-daily cadence (≈5-min ephemeral TTL
  expires between runs). It only pays off for runs <5 min apart (retries/testing)
  or a higher-frequency schedule. Revisit if the schedule changes.

## 7. Telegram confirm loop (#1) — post-implementation follow-ups

Item #1 is **implemented** (see `docs/superpowers/specs|plans/2026-06-01-telegram-confirm-loop-*.md`).
Small follow-ups surfaced by the final review (spec/plan for the fixes at
`docs/superpowers/specs|plans/2026-06-02-backlog-6-7.md`):

- **`encode_callback` `debug_assert!(len <= 64)`** (`src/telegram.rs`) — **DONE (2026-06-02)**.
  `encode_callback` now returns `Option<String>` (`None` when `<action>:<key>` exceeds
  64 bytes). `build_prompt` drops the entire inline keyboard when the key won't fit
  (all-or-nothing, since every button shares the key); the held entry stays resolvable
  via free-text reply (correlated by `telegram_message_id`) and the state-file fire-exit.
  `build_send_request` now omits `reply_markup` entirely when there are no buttons
  (it previously emitted an empty `[[]]` keyboard that real Telegram rejects).
- **Stale Telegram prompt on failed remap** (`src/run.rs` remap-normalisation) —
  **DONE (2026-06-02)**. The `None` arm now best-effort `editMessageText`s the superseded
  prompt to a plain-text breadcrumb before clearing `telegram_message_id`, so its old
  buttons can't be tapped. Sent as plain text (no `parse_mode`) so a user remap containing
  Markdown specials can't 400 the edit and silently leave the buttons live. A failed edit
  only `warn!`s; it never aborts the run.
- The `create_payee`→`import` transactional hazard (item #4) is now also reachable via the
  Telegram create-new flow — **resolved under #4** (concluded self-healing: the review
  survives on disk, so a failed import retries cleanly next run).
