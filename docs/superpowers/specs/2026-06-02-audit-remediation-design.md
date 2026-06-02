# Audit remediation — design (Phase 0 + Phase 1 + cheap secret hardening)

**Date:** 2026-06-02
**Source:** `docs/AUDIT-2026-06-02.md` (pre-deploy audit; verdict NO-GO).
**Goal:** Clear the deploy blockers and Phase-1 hardening so `aba`'s hard invariants are
*enforced* rather than merely intended, making the first production deploy justified.

**Scope (16 items):** C1, H1, H2, H3 (+ 3 regression tests); M1–M9; L4, L5, L6.
Out of scope this cycle: L1–L3, L7, L8, the deps/test cluster, and the Info items
(deferred — see audit §6 Phase 2). H2 resolved as **plain text** (decision: reliability
over chat styling; no recurring escaper footgun; consistent with the #7b edit path).

---

## Theme A — Fail-loud: never silently drop a transaction (C1, M1, M4, M5)

### C1 (critical) — eliminate the silent matched-drop
`apply_matches` (`src/run.rs`) currently pushes a Claude match into `matched` on
confidence alone, with no check that the payee exists in Actual. A high-confidence
match to a non-existent payee then hits the `(Some(_), None)` arm at `run.rs:324` and is
dropped with only a `warn!`, while `last_run` still advances → permanent loss.

- Add a parameter to `apply_matches` carrying the set of existing payee names (e.g.
  `existing: &BTreeSet<&str>` built from `payee_names`). The auto-import arm becomes:
  `Some(payee) if m.confidence.rank() >= autoimport.rank() && existing.contains(payee.as_str()) => matched`,
  else `Some(payee) => proposed` (held for review with `suggestion = payee`). A
  non-existent high-confidence payee is now **held**, not dropped, and the user gets the
  existing "No existing payee *X*. Create it?" prompt.
- The Claude `(Some(_), None)` arm at `run.rs:324-326` becomes unreachable; replace its
  body with `unreachable!`/`debug_assert!` documenting that `apply_matches` guarantees
  every `matched` payee exists, OR keep a defensive hold. (Plan picks one; prefer
  routing-to-review for defence in depth.)
- **Sibling fix (same theme):** the cache-path arm at `run.rs:302-304` has the identical
  bug — a *cached* payee deleted from Actual is dropped with a `warn!`. Route it to a
  pending review too (hold the tx with the cached name as `suggestion`) instead of
  dropping. Cheap, removes the known twin.

### M1 — fetch-start cursor, not end-of-run wall-clock
Capture `let fetch_start = Utc::now();` immediately before `bank.fetch_since(since)`. On a
fully successful run set `state.last_run = Some(fetch_start)` (instead of `Utc::now()` at
the end). A transaction that arrives *during* the run is then re-fetched next run instead
of falling into a gap; dedup via `imported_id` prevents double imports.

### M4 / M5 — loud schedule drops
`schedules_from_rows` (`src/run.rs`) must `warn!` and explicitly skip a schedule row with
(M4) an unparseable/missing date, and (M5) a missing/empty payee — instead of silently
dropping (M4) or coercing to `""` so it never matches (M5). A silently dropped schedule
causes a real transaction to be imported as a duplicate of a scheduled one.

---

## Theme B — Availability: timeouts everywhere (H1)

No `reqwest` client and no shim wait has a timeout; an unattended one-shot cron with no
watchdog hangs forever, holding the lock and stalling every later run.

- Add a shared client builder (e.g. `fn http_client(total: Duration) -> reqwest::Result<Client>`
  in a small `src/http.rs` or as a `Config` helper) applying `connect_timeout(10s)` and a
  caller-chosen total `timeout`. Replace all `reqwest::Client::new()` sites:
  - Monzo `30s`, Telegram `30s`, Notify `15s`, **Claude `120s`** (LLM latency), and the
    `run.rs` OAuth token-exchange client `30s`.
- Wrap the shim `child.wait_with_output()` (`src/actual/mod.rs`) in
  `tokio::time::timeout(Duration::from_secs(120), ...)`; on elapse, `child.kill().await.ok()`
  and return `Err` (loud, lock released on process exit).
- Add a top-level `tokio::time::timeout` (~`600s`) around the `run()`/`poll()` call in
  `src/main.rs` as a backstop; on elapse, exit non-zero with a clear error.

---

## Theme C — Telegram robustness (H2, M2)

### H2 — plain-text prompts (no Markdown)
Bank `raw_merchant` and Claude `suggestion` are interpolated into Markdown v1; a name with
unbalanced `` ` ``/`*`/`_` makes Telegram 400 the send, which is swallowed → the held
transaction is stuck and re-fails forever.

- `build_send_request` (`src/telegram.rs`): remove the `"parse_mode": "Markdown"` field
  (the buttons/`reply_markup` logic is unchanged).
- `build_prompt` (`src/run.rs`): replace `*bold*` / `` `code` `` / `_italic_` styling with
  plain `'single-quoted'` names. All three flavors keep their wording, minus the markup.
- Both Telegram paths (prompt + the #7b edit) are now plain text — one convention.
- Update the affected tests: `build_send_request_has_text_and_buttons` (assert no
  `parse_mode`), `build_prompt_picks_flavor_by_payee_membership` (plain-text chat_text).

### M2 — redact bot token from Telegram error bodies
The three `anyhow!("telegram error {status}: {value}")` sites (getUpdates, sendMessage,
editMessageText) interpolate the raw response body while every *other* error arm uses
`self.redact(...)`. Route `value` through `self.redact(value.to_string())` in all three.
Add a test asserting the token never appears in a non-2xx error chain.

---

## Theme D — Secrets (H3, L4, L5, L6)

### H3 — pipe + redact shim stderr
`src/actual/mod.rs` spawns the shim with `stderr(Stdio::inherit())` while `ACTUAL_PASSWORD`
is in its env; `@actual-app/api` (third-party) may emit auth diagnostics to stderr →
container logs. Switch to `stderr(Stdio::piped())` (captured by the existing
`wait_with_output()`), redact the password substring from the captured stderr, and emit it
only on non-zero exit via `tracing::debug!`/`error!` (never on success).

### L4 / L5 — hand-written `Debug` that redacts
Replace `#[derive(Debug)]` on `Config` (`src/config.rs`), `State` and `MonzoState`
(`src/state.rs`) with manual `impl Debug` that masks secret fields (`refresh_token`,
`monzo_client_secret`, `anthropic_api_key`, `actual_password`, telegram token) as `***`
while showing non-secret fields (e.g. `account_id`, `last_run`). Keeps `Debug` available
for tests/assertions; closes the latent `dbg!`/`{:?}` dump risk.

### L6 — redact shim error messages
- In `actual-shim/index.js`: add a `redact(msg)` that replaces `process.env.ACTUAL_PASSWORD`
  (when set & non-empty) with `***` in any error string before it is written to stdout JSON.
- On the Rust side, redact the password from the `anyhow!("shim error: {error}")` string in
  `ActualClient` (belt-and-suspenders, since the client holds the password).

---

## Theme E — Resilience & infra (M3, M6, M7, M8, M9)

### M3 — strict env confidence parse
`ABA_AUTOIMPORT_CONFIDENCE` currently degrades a typo (`High`, `MEDIUM`) to `Medium`,
silently widening auto-import. Add a strict `Confidence::from_env_str(&str) -> anyhow::Result<Confidence>`
used on the env path (in `Config`/`main.rs`); a bad value is a hard config error. Keep the
existing lenient parser for Claude API responses (where over-strictness would abort a run).

### M6 — persist message ids durably after the send loop
After the prompt-send loop in `run()`, persist state (the freshly-set
`telegram_message_id`s) before any further fallible/best-effort work, so a later failure
can't strand them and cause duplicate prompts next run. (UX-only fix; not data loss.)

### M7 — create the lock-file parent dir
`src/lock.rs` must `create_dir_all(parent)` before `File::create`, mirroring `State::save`,
so a nested `ABA_LOCK_PATH` (or a poll-first fresh volume) doesn't fail with `NotFound` and
block both subcommands.

### M8 — robust atomic state write
`State::save` (`src/state.rs`) uses a fixed `.tmp` sibling + `rename`, which fails on a
cross-filesystem path (EXDEV). Switch to `tempfile::NamedTempFile` in the target dir +
`.persist(path)`. Promote `tempfile` from dev-dependency to a regular dependency. Behaviour
on failure stays loud (`?` propagates; `last_run` not advanced) — invariant preserved.

### M9 — pin Docker base images
`Dockerfile` uses floating `rust:1-slim` / `node:22-slim`. Pin to specific version +
`@sha256:` digests for reproducibility. Digests obtained via `docker manifest inspect` at
implementation time; if the registry is unreachable, fall back to pinned patch tags (e.g.
`node:22.x.y-slim`) and leave a `# TODO: add @sha256 digest` note.

---

## Testing

**Phase-0 regression tests (lock the blockers):**
1. C1: a high-confidence Claude match to a payee NOT in the live list is **held for
   review** (pending_review with that suggestion), not dropped; `imports.log` does not
   contain it; `last_run` still advances (the tx is safely held, not lost).
2. H2: a `build_prompt` for a merchant containing `` ` ``/`*`/`_` produces plain chat_text
   and `build_send_request` emits no `parse_mode`; a wiremock `send_prompt` with such a
   name succeeds (200) rather than erroring.
3. Fail-loud: a `run()` whose import fails leaves `last_run == None` (assertion the audit
   noted was missing; prod code already correct).

**Per-fix unit tests:** shared client builder sets timeouts (assert `connect_timeout`/`timeout`
present, or behaviour via a non-routable host returning a timeout error fast); M2 token
redaction in the three error arms; M3 strict parse rejects a bad value and accepts
high/medium/low; M4/M5 loud schedule drops (warn + skipped, not silently coerced); L4/L5
`Debug` output contains `***` and not the secret; M7 lock works when the parent dir is
missing; M8 save round-trips via `persist`.

**Verification:** `cargo build && cargo test && cargo clippy --all-targets` (trust cargo,
not IDE diagnostics — diagnostics on this repo are stale). Shim: a manual `npm`/node smoke
that a forced error redacts the password. Re-confirm the C1 and H2 paths by reading the
final code.

---

## Files touched

- `src/run.rs` — C1 (`apply_matches` + drop arms), M1 (fetch-start cursor), M4/M5
  (`schedules_from_rows`), H2 (`build_prompt`), M6 (post-send save), timeout client wiring.
- `src/telegram.rs` — H2 (`build_send_request`), M2 (redact error bodies), timeout client.
- `src/actual/mod.rs` — H3 (piped+redacted stderr), shim timeout, L6 (Rust-side redact).
- `actual-shim/index.js` — L6 (JS redact).
- `src/claude.rs`, `src/banks/monzo.rs`, `src/notify.rs` — timeout clients; M3 (`Confidence`).
- `src/config.rs` — L5 (`Debug`), M3 (strict env parse wiring).
- `src/state.rs` — L4 (`Debug`), M8 (`tempfile` persist).
- `src/lock.rs` — M7 (`create_dir_all`).
- `src/main.rs` — H1 top-level timeout backstop; M3 wiring.
- `src/http.rs` (new, optional) — shared timeout client builder.
- `Cargo.toml` — promote `tempfile` to dependency.
- `Dockerfile` — M9 (pin base images).

## Non-goals / deferred (audit Phase 2)
L1 (orphan payee — self-heals), L2 (shim exit status), L3 (`ShimResponse` flat struct),
L7 (`rust-version` floor), L8 (OAuth listener timeout), `npm audit`/`cargo audit` in CI,
exact `@actual-app/api` pin, `auth_monzo` → `src/auth.rs`, missing test variants beyond the
ones above.
