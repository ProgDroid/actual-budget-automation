# Phase 2 backlog #6 + #7a + #7b — design

**Date:** 2026-06-02
**Scope:** Three deferred follow-ups from `docs/PHASE2-BACKLOG.md`:

- **#6** — Operational notes (documentation only, no code).
- **#7a** — `encode_callback` over-long `callback_data` handling.
- **#7b** — Stale inline-keyboard cleanup when a free-text remap re-holds an entry.

Out of scope (by decision): automating the `chown` step (#6 second bullet stays
docs-only); the `create_payee`→`import` transactional hazard (#7 third bullet) —
already concluded self-healing under backlog #4.

---

## #6 — RUNBOOK documentation (no code)

Two additions to `docs/RUNBOOK.md`. Pure documentation; no code or scripts.

### 6.1 Volume ownership (uid 1000)

The runtime container runs as the non-root `node` user (uid 1000, set by
`USER node` in the `Dockerfile`). The host bind-mount (`/srv/aba` → `/data`)
holds both the state file (`ABA_STATE_PATH=/data/aba.state.toml`) and the Actual
cache (`ACTUAL_DATA_DIR=/data/actual-cache`). If the host directory is not
writable by uid 1000, the container cannot persist state or cache and the run
fails.

Document a one-time ownership step **before** the first `docker run`, in the
"Daily run" section:

```sh
sudo mkdir -p /srv/aba
sudo chown -R 1000:1000 /srv/aba
```

A short sentence explains *why* (container runs as uid 1000; bind-mount must be
writable by that uid).

### 6.2 Prompt-caching cadence caveat

Add a short note (new "Operational notes" subsection, or near the env-var
table): Anthropic prompt caching uses an ~5-minute ephemeral TTL that expires
between once-daily runs, so it provides **no benefit at the nightly cadence**.
It only pays off for runs <5 minutes apart (retries / local testing) or a
higher-frequency schedule. Revisit if the schedule changes.

---

## #7a — `encode_callback` over-long key

### Problem

`callback_data` has a hard **64-byte** cap in the Telegram Bot API. The encoding
is `"{action}:{entry_key}"` where `action` ∈ {`cf`, `nw`, `sk`} (2 bytes) + `:`
= 3 bytes of overhead, leaving ~61 bytes for `entry_key` (the transaction
`external_id`). Monzo external_ids (~30 chars) fit comfortably; a future source
with longer ids (e.g. GoCardless, backlog #2) could exceed the cap.

Today `encode_callback` uses `debug_assert!(s.len() <= 64, …)`: it panics in
debug/test builds and, in release, silently emits an over-long payload that
**Telegram rejects**, failing the whole `send_prompt`.

Truncating the key is *not* a safe fix on its own: Telegram echoes the
`callback_data` verbatim on a button tap, and a truncated key no longer matches
any `pending_review.external_id`, so the resolution silently vanishes.

### Approach: `Option` return + omit buttons (graceful degradation)

The project already has two correlation paths and a fire-exit, so dropping the
inline keyboard for an over-long key degrades to a still-working free-text path
with no silent data loss.

- **`encode_callback(action, entry_key) -> Option<String>`** in `src/telegram.rs`.
  Returns `None` when `format!("{action}:{entry_key}").len() > 64` (`str::len`
  is the UTF-8 **byte** length, matching Telegram's byte cap), `Some(s)`
  otherwise.

- **`build_prompt`** (`src/run.rs`): build the button list through the encoder.
  Because every button in a prompt shares the same `entry_key`, encoding is
  all-or-nothing — if any `encode_callback` returns `None`, emit the prompt with
  `buttons: vec![]`. The chat text already invites a free-text reply in every
  flavor ("Reply with a payee name" / "Reply with a different payee to remap"),
  so the entry stays resolvable: it is still held and still correlatable via
  `msg_map` (`telegram_message_id` → `external_id`) on a free-text reply, and the
  state-file fire-exit still applies.

- **`build_send_request`** (`src/telegram.rs`): omit `reply_markup` **entirely**
  when `buttons` is empty. Today it unconditionally emits
  `"inline_keyboard": [row]`, which for an empty button list becomes `[[]]` — an
  empty keyboard that real Telegram rejects. This is required both for the new
  empty-button prompt above and as a correctness fix in its own right.

### Tests

- `callback_roundtrips` updates to the `Option` return; assert
  `encode_callback("cf", "tx_0001") == Some("cf:tx_0001")`.
- New: an `entry_key` that pushes the payload over 64 bytes yields `None`.
- New: `build_prompt` with an over-long `external_id` yields a prompt with no
  buttons (and still-populated chat text).
- New / updated: `build_send_request` with empty buttons produces no
  `reply_markup` key; with buttons it still produces the keyboard (existing
  `build_send_request_has_text_and_buttons` covers the non-empty case).

---

## #7b — Stale inline-keyboard cleanup on re-hold

### Problem

In `run.rs`'s remap-normalisation loop, when a free-text remap reply does **not**
fuzzy-match the live payee list, the entry is re-held as a "create it?" prompt:
`suggestion` is set, `resolve_to` is cleared, and `telegram_message_id` is set to
`None` so a fresh prompt is sent next run (`src/run.rs:227-228`). But the
*previous* inline-keyboard message still sits in the chat with its original,
still-valid buttons — tapping one would apply a stale action to the (now
re-held) entry. Harmless for a single user, but confusing.

### Approach: `editMessageText` to a "superseded" note

Edit (not delete) the stale message: `editMessageText` has no 48-hour limit
(unlike `deleteMessage`), removes the inline keyboard by simply omitting
`reply_markup`, and leaves a breadcrumb in the chat.

- **`ConfirmChannel` trait** (`src/telegram.rs`) gains:
  ```rust
  async fn edit_message_text(&self, message_id: i64, text: &str) -> anyhow::Result<()>;
  ```
  Implemented for `HttpTelegram` via a POST to `editMessageText` with
  `{ chat_id, message_id, text, parse_mode: "Markdown" }` and **no**
  `reply_markup` (which clears the old keyboard). Errors are redacted via the
  existing `redact` helper, consistent with `send_prompt` / `poll_replies`.

- **`run.rs` remap-normalisation `None` arm**: before clearing
  `telegram_message_id`, if a `channel` is present and an old `message_id`
  exists, best-effort call
  `channel.edit_message_text(old_id, "↪️ Couldn't match '<reply>' — re-asking below.")`.
  Failure only `warn!`s and never aborts the run, consistent with the existing
  best-effort Telegram patterns (`send_prompt failed (will retry next run)`,
  `answerCallbackQuery` ack). The loop borrows `&mut state.pending_reviews` while
  awaiting on the shared `&dyn ConfirmChannel` — no borrow conflict.

- **Test double**: `run.rs`'s `FakeChannel` (the only `ConfirmChannel` double;
  `poll.rs` reuses it via `crate::run::tests::FakeChannel`) implements the new
  trait method, recording calls in a new `edited: Mutex<Vec<(i64, String)>>`
  field so tests can assert on them.

### Tests

- A `run()` test: a re-held remap with an existing `telegram_message_id` fires
  one `edit_message_text` against that id, and `telegram_message_id` ends up
  `None`.
- A failing `edit_message_text` does not abort the re-hold (entry is still
  re-held; run completes `Ok`).
- `HttpTelegram::edit_message_text` against a `wiremock` mock posts to
  `editMessageText` with the expected body and no `reply_markup`.

---

## Files touched

- `docs/RUNBOOK.md` — #6 docs.
- `src/telegram.rs` — `encode_callback` → `Option`; `build_send_request`
  conditional `reply_markup`; `ConfirmChannel::edit_message_text` +
  `HttpTelegram` impl; tests.
- `src/run.rs` — `build_prompt` empty-button path; remap-normalisation edit
  call; test double + tests.
- `src/poll.rs` — no change needed; it reuses `run.rs`'s `FakeChannel`.

## Non-goals / deferred

- No `chown` automation script (docs only).
- No change to the `parse_get_updates` correlation core (Option-2 redesign and
  Option-3 hash-token approaches were considered and rejected as over-scoped).
- No action on the `create_payee`→`import` hazard (resolved under #4).
