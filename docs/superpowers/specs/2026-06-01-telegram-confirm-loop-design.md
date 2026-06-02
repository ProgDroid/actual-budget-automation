# Telegram Confirm/Correct Loop — Design

**Date:** 2026-06-01
**Status:** Approved for planning
**Phase 2 backlog item:** #1 (see `docs/PHASE2-BACKLOG.md`)

## Overview

Add an asynchronous, two-way Telegram channel so that uncertain Claude payee
matches can be **confirmed or corrected from a phone** before they are imported
into Actual and folded into the sticky `payee_cache`. The same channel resolves
the existing "no match — held for review" entries, making Telegram the canonical
resolution UX. Editing the TOML state file by hand is demoted to a documented
emergency fallback (the "fire exit").

The system stays true to the Phase 1 architecture: **one-shot containers, zero
idle memory**. No resident daemon and no webhook listener are introduced.
Instead the work is split across two cron-triggered one-shot subcommands.

## Goals

- Catch a *wrong* Claude match before it becomes a permanent cache entry.
- Resolve held items (proposed or no-match) from the phone, end to end.
- Preserve the core invariant: **never silently auto-create or auto-import a
  payee the user has not vetted** (high-confidence matches excepted, as today).
- Preserve **zero idle memory**: every process remains a short-lived one-shot.
- **No reply is ever lost** to Telegram's 24h `getUpdates` retention window.
- Telegram is optional: if unconfigured, held items still queue and are
  resolvable via the TOML fire exit. Nothing breaks.

## Non-goals (this feature)

- Fixing the pre-existing `create_payee`→`import` transactional hazard
  (backlog #4). The Telegram create-new flow reaches the same code path; this is
  flagged as a known risk to address with #4, not here.
- Near-real-time import into Actual. Confirmed transactions land on the next
  nightly run (latency tolerated by decision; see "Poll resolves, run imports").
- A resident process, webhook server, or scheduler change.
- GoCardless, Monzo pagination, or other Phase 2 items.

## Why asynchronous across runs (the architecture crux)

The container must never block waiting for a human reply — a parked container is
a resident daemon in disguise and forfeits the zero-idle-memory property that
justified the one-shot model. So the loop is **asynchronous across runs**: a
reply sent during the day is read on a later run.

Telegram's `getUpdates` queue is the durable mailbox. A button tap or text reply
sits on Telegram's servers until something calls `getUpdates`, and **is not kept
longer than 24 hours**. Polling only once per night is therefore *lossy*: a reply
made shortly after a run sits ~24h until the next poll (cliff edge), and a single
missed nightly run (reboot, cron hiccup, the Monzo re-auth early-return path)
pushes the gap past 24h — the reply is **silently dropped**.

**Resolution: split into two one-shot subcommands with different cadences.**

```
*/15 * * * *  docker run --rm … aba poll   # drain Telegram mailbox, apply replies
0   3 * * *   docker run --rm … aba run    # fetch → match → import, send new prompts
```

Both stay one-shot (0 MB between invocations). An empty poll is a single fast
`getUpdates` HTTP call.

### Responsibility split

| Capability | `aba poll` (~15 min) | `aba run` (nightly) |
|---|---|---|
| Reads Telegram (`getUpdates` + offset) | **yes — sole owner** | no |
| Sends Telegram | acks / feedback only | new prompts for today's medium/low |
| Reads Actual (payees, schedules) | no | yes |
| Writes Actual (import, `create_payee`) | no | **yes — sole writer** |
| Calls Claude / Monzo | no | yes |
| State lock | try-lock, **skip** if busy | blocking lock, **always proceeds** |
| Dependencies | Telegram + state file | Monzo, Actual+shim, Claude, Telegram-send, state |

Rationale for this split (decided):

- **Single writer to Actual.** All money-touching writes — including the known
  `create_payee`→`import` hazard — stay in one place, run once nightly, on a
  predictable schedule. The poller never writes anything irreversible.
- **The poller is trivially boring.** Telegram + state file only. Cheap to run
  every 15 min; if Actual is down it does not care. It records *intent*; the
  nightly run *executes* it.
- **Intent and execution are cleanly separated.** The tap durably records
  `resolve_to`/`create_new` into state; the write happens later in the controlled
  nightly batch with its existing summary/notification machinery.
- **Cost is pure latency-to-Actual** (≤ ~24h), explicitly tolerated. The
  expiry/loss risk is solved by frequent polling regardless.

## Confidence-gated matching

`PayeeMatch` (`src/claude.rs`) gains a `confidence` field. The `report_matches`
tool schema gains a **required ordinal enum** `confidence: "high" | "medium" |
"low"` per entry, with system-prompt guidance describing each level. Numeric
scores are deliberately avoided (LLM numeric confidence is poorly calibrated).

Gating on fresh Claude matches (cache hits are always imported — already vetted):

- **`high`** → import + cache silently, exactly as Phase 1. Reported in the
  summary only.
- **`medium` / `low`** → **held** as a `pending_review` carrying Claude's
  suggestion + confidence. **Not imported.** A Telegram prompt is sent.

The auto-import cutoff is config-overridable (`ABA_AUTOIMPORT_CONFIDENCE`,
default `high`), but high-only is the intended default.

## Data model changes

All additions are `Option` / `#[serde(default)]`, so existing state files load
unchanged.

### `PendingReview` (`src/state.rs`) — three new fields

```rust
pub suggestion: Option<String>,        // Claude's proposed payee (medium/low); None = no-match
pub confidence: Option<String>,        // "medium" | "low"
pub telegram_message_id: Option<i64>,  // Some = prompt already sent (idempotency)
```

One struct now covers both kinds of held item:
- **Proposed** (`suggestion = Some`): Claude matched but with medium/low
  confidence.
- **Unmatched** (`suggestion = None`): no match found (today's behavior).

### `PayeeMatch` (`src/claude.rs`)

```rust
pub confidence: Confidence,   // High | Medium | Low (enum)
```

### New top-level state

```toml
[telegram]
last_update_id = 0    # getUpdates offset, owned and advanced by `aba poll`
```

## Module layout

New module `src/telegram.rs` behind a trait (mirrors `Notifier` /
`ClaudeMatcher` for testability):

```rust
pub enum ReplyAction { Confirm, CreateNew, Skip, Remap(String) }
pub struct Reply { pub entry_key: String, pub action: ReplyAction }
pub struct Prompt { pub chat_text: String, pub buttons: Vec<(String, String)> }

#[async_trait]
pub trait ConfirmChannel: Send + Sync {
    /// Drain updates newer than `since`. `msg_map` maps a prompt's message_id to
    /// the entry it was sent for, so free-text replies can be correlated.
    /// Returns (applied replies, new offset to persist).
    async fn poll_replies(
        &self,
        since: i64,
        msg_map: &BTreeMap<i64, String>,
    ) -> anyhow::Result<(Vec<Reply>, i64)>;
    /// Send a prepared prompt; returns the Telegram message_id.
    async fn send_prompt(&self, prompt: &Prompt) -> anyhow::Result<i64>;
}
```

**Implementation note:** `send_prompt` takes a prepared `Prompt` (built by `run`,
which has access to the payee list), not a raw `&PendingReview`. The channel is
transport-only. Similarly, `poll_replies` takes an explicit `msg_map`
(`message_id → external_id`) for free-text reply correlation, because the HTTP
transport must correlate `reply_to_message.message_id` against sent prompts.

The HTTP impl (`HttpTelegram`) hides whether a reply arrived as a `callback_query`
(button) or a `message` (free text), so the command logic stays
transport-agnostic. Config override `TELEGRAM_BASE_URL` for tests.

`entry_key` = the transaction `external_id` (Monzo ids ~30 chars, within
Telegram's 64-byte `callback_data` cap).

## Telegram UX & reply correlation

**Per-item message** (each held item is its own message, so `reply_to_message`
and buttons correlate cleanly). Two prompt flavors:

- **Proposed** (`suggestion = Some`):
  > `TFL TRAVEL CH` → **Transport for London**? (medium confidence)

  Buttons: `✅ Confirm` · `🆕 Create new` · `❌ Skip`. Or reply with a different
  payee name to remap.
- **Unmatched** (`suggestion = None`):
  > New merchant `SQ *SOME CAFE` — which payee?

  Buttons: `🆕 Create new` · `❌ Skip`. Reply with a payee name to map.

**`callback_data` encoding:** `<action>:<external_id>` — e.g. `cf:tx_0000A…`,
`nw:tx_…`, `sk:tx_…`. On tap, `answerCallbackQuery` clears the client spinner.

**Reply → `ReplyAction` mapping (in `aba poll`, write-free):**

| Input | Action | State effect (poll) | Ack |
|---|---|---|---|
| `✅ Confirm` | `Confirm` | `resolve_to = suggestion` (already an existing payee name) | "✓ Confirmed" |
| `🆕 Create new` | `CreateNew` | `create_new = true` | "✓ Will create" |
| `❌ Skip` | `Skip` | remove the pending entry; do not import (merchant may recur later) | "Skipped" |
| free text `"Sainsbury's"` | `Remap(text)` | `resolve_to = text` **verbatim** (not yet matched) | "✓ noted, will map 'Sainsbury's' tonight" |

**Free-text remap is resolved by `aba run`, not the poller.** The poller has no
payee list, so it records the typed text verbatim and acks. The nightly run —
which already loads the payee list (`run.rs:142`) — fuzzy-matches it (reusing
`matching.rs` similarity):

- Strong match → import under that payee + cache.
- No good match → re-held with `suggestion = <typed text>` and `telegram_message_id`
  cleared; on the next run a fresh prompt is sent in a **"create it?" flavor**:
  > No existing payee **Sainsbury's**. Create it? `🆕 Create` · `❌ Skip`

  This flavor is driven by `build_prompt` in `run.rs`, which keys on whether the
  `suggestion` value is present in the live payee list. A suggestion that is not a
  known payee produces the two-button Create/Skip prompt instead of the
  three-button Confirm/Create/Skip prompt.

`Confirm` needs no fuzzy-match: `suggestion` was chosen by Claude *from* the
existing payee list, so `resolve_to` is already an exact name (the run validates
it via `payee_id_for`, warning if it has since vanished).

## `aba run` pipeline (revised)

1. `ensure_auth` (unchanged; Monzo re-auth early-return unchanged).
2. Acquire **blocking** state lock.
3. Fetch since `last_run` → scheduled-skip (unchanged).
4. Read Actual payees + schedules (unchanged) — needed by both the remap
   fuzzy-match (step 5) and the import step.
5. **Apply already-resolved pending reviews** via `take_resolved()` — this now
   includes entries the poller resolved during the day. For each, if `resolve_to`
   is free-text from a remap, fuzzy-match against the payee list from step 4;
   clean match → import; no match → re-hold with a fresh prompt.
6. Cache split → Claude match (now confidence-tagged).
7. **Partition fresh matches:** `high` → import + cache; `medium`/`low` → held as
   `pending_review` with `suggestion` + `confidence` (**not** imported).
8. Unmatched → `pending_review` with `suggestion = None`.
9. Import resolved + high-confidence rows (sole writer).
10. **Send prompts** for every `pending_review` whose `telegram_message_id` is
    `None`; store the returned id (idempotent — never re-prompts the same entry).
11. Advance `last_run`; save state; release lock.
12. Apprise summary, now appended with **"N items awaiting your confirmation"**
    (the prompt-once + reminder-count behavior; never auto-imports, never
    per-item nags).

`run` does **not** call `getUpdates` — only the poller does — so sending prompts
never contends over the offset.

## `aba poll` pipeline (new)

1. Acquire state lock with **try-lock**; if busy, log and exit 0 (next poll in
   ~15 min).
2. `poll_replies(last_update_id)`:
   - Discard any update whose sender chat id ≠ `TELEGRAM_CHAT_ID` (allowlist).
   - Discard any reply whose `entry_key` is not in the current pending list
     (stale / spoofed).
   - Map remaining updates to `ReplyAction`s and apply to state (table above).
   - Send the per-reply ack; `answerCallbackQuery` for button taps.
3. Advance `last_update_id` to the highest applied update id; save state; release
   lock.

The poller never reads or writes Actual, never spawns the shim, never calls
Claude or Monzo. On Telegram failure it does not advance the offset and exits
non-zero so cron surfaces it; the next poll retries (no reply lost — still within
the 24h window given the 15-min cadence).

## Concurrency & locking

`poll` (~15 min) and `run` (nightly) overlap at most once a day. An OS file lock
on `<state>.lock` (e.g. via `fs2`) serializes them:

- `run` takes a **blocking** lock (with a sane timeout) — it must always proceed.
- `poll` uses **try-lock** and exits cleanly if it can't acquire — another poll
  follows shortly.

The existing atomic temp-file + rename write (`state.rs:56`) prevents corruption;
the lock prevents lost updates from interleaved read-modify-write.

## Dry-run

`--dry-run` applies to `run` only and is unchanged in spirit: no Actual writes,
and additionally **no Telegram sends** (posting a prompt is a real mutation). The
existing dry-run state-splice guarantee holds — only the rotated Monzo token is
persisted. `poll` has no dry-run mode (it is inherently low-risk: no Actual
writes), and is excluded from dry-run entirely.

## Security (single-user hardening)

- **`chat_id` allowlist:** `poll_replies` discards every update whose sender ≠
  configured `TELEGRAM_CHAT_ID`. Stops a stranger who finds the bot from
  injecting resolutions.
- **`entry_key` validation:** replies referencing an unknown pending entry are
  ignored.
- **Bot token is an env secret** (`TELEGRAM_BOT_TOKEN`); never logged. The state
  file already holds the rotating Monzo token and is gitignored / volume-mounted.

## Configuration / secrets (new env vars)

- `TELEGRAM_BOT_TOKEN` — bot API token (secret).
- `TELEGRAM_CHAT_ID` — the only chat allowed to drive resolutions.
- `ABA_AUTOIMPORT_CONFIDENCE` — auto-import cutoff, default `high`.
- `ABA_LOCK_PATH` — optional; defaults to `<state>.lock` beside the state file.
- `TELEGRAM_BASE_URL` — test override for the Telegram API base.

If `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` are unset, the Telegram channel is
disabled: `run` still holds medium/low and no-match items (resolvable via the
TOML fire exit) and skips prompt-sending; `poll` exits as a no-op.

## Operational notes

- Two cron lines now (poll + run) instead of one. Both one-shot, both 0 MB idle.
- The `/data` volume must remain writable by uid 1000 (existing note); the
  `<state>.lock` file lives there too.
- Document the bot setup: create the bot via BotFather, obtain the token, send it
  one message, read your `chat_id` from `getUpdates` once during setup.

## Testing approach (TDD, trait doubles)

`ConfirmChannel` fake (like the existing `Notifier` / `ClaudeMatcher` doubles):

1. **Confidence partition** — `high` imports + caches; `medium`/`low` held with
   `suggestion`; no-match held with `suggestion = None`.
2. **Claude schema** — `build_request` emits the `confidence` enum;
   `parse_response` reads it; round-trips high/medium/low.
3. **Poll → resolve → run-import ordering** — a reply applied by `poll` is
   imported by the next `run` (the async-across-runs guarantee).
4. **Idempotent prompt-send** — entry with `telegram_message_id = Some` is never
   re-prompted.
5. **Free-text remap deferral** — poller stores text verbatim; run fuzzy-matches;
   no-match path re-holds with a fresh prompt.
6. **`chat_id` filtering** — updates from other chats are discarded.
7. **`entry_key` validation** — replies for unknown entries are ignored.
8. **Locking** — `poll` try-lock skips when `run` holds the lock; `run` proceeds.
9. **Dry-run** — `run --dry-run` performs zero Telegram sends and zero Actual
   writes; only the rotated Monzo token persists.
10. **`run` integration** — composes the doubles end to end (ties into backlog #4
    coverage).

## Open items / known risks

- **`create_payee`→`import` transactional hazard** (backlog #4) is now reachable
  via the Telegram create-new flow. Out of scope here; address with #4.
- **Skip semantics:** `Skip` drops the pending entry and does not cache the
  merchant, so the same merchant can be re-evaluated (and re-prompted) if it
  recurs. Intended: skip means "not this transaction," not "ignore forever."
- **Stale Telegram prompt:** when a failed free-text remap clears
  `telegram_message_id` to re-prompt, the previous inline-keyboard message
  remains in the chat with stale buttons (still carrying the same `external_id`
  callback). Harmless in the single-user flow but could confuse; a future cleanup
  could `editMessageText`/`deleteMessage` the old prompt. Deferred.
