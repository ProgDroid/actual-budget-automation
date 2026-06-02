# Audit Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clear the pre-deploy audit's blockers (C1, H1–H3) and Phase-1 hardening (M1–M9) plus cheap secret hardening (L4–L6) so `aba`'s hard invariants are enforced, not merely intended.

**Architecture:** Targeted, localized fixes across `run.rs`/`telegram.rs`/`actual`/`state`/`lock`/`config`/`claude`/`main` + the Node shim + `Dockerfile`. No structural redesign — every fix is a missing guard. Plain-text Telegram (no Markdown). Shared timeout-bearing HTTP client. Fail-loud: a transaction is always imported OR held, never dropped.

**Tech Stack:** Rust (anyhow, reqwest 0.12, tokio, serde, toml, tempfile, chrono), Node shim. Spec: `docs/superpowers/specs/2026-06-02-audit-remediation-design.md`. Audit: `docs/AUDIT-2026-06-02.md`.

**Implementer notes (read first):**
- Run from repo root `G:/rustDev/actual-budget-automation` (Windows). **This is a binary crate — tests run with `cargo test`, NOT `cargo test --lib` (which errors "no library targets").**
- **Trust `cargo build`/`cargo test`/`cargo clippy --all-targets` only — rust-analyzer IDE diagnostics on this repo are frequently STALE.** Baseline before you start: 78 tests pass.
- Branch `feat/audit-remediation` is already checked out (spec is its first commit). One commit per task with the exact message given; append:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- Follow TDD: failing test → run it red → implement → run green → commit. Each task ends with the whole suite green.

---

## File Structure

- `src/http.rs` — **new**: shared `reqwest::Client` builder with timeouts (Task 1).
- `src/run.rs` — C1 (`apply_matches` + drop arms), M1 (fetch-start cursor), M4/M5 (`schedules_from_rows`), M6 (post-send save), H2 (`build_prompt`), apply_matches call site.
- `src/telegram.rs` — H2 (`build_send_request`), M2 (redact error bodies), timeout client.
- `src/actual/mod.rs` — H1 (shim timeout + `kill_on_drop`), H3 (piped+redacted stderr), L6 (Rust-side redact).
- `actual-shim/index.js` — L6 (JS redact).
- `src/claude.rs`, `src/banks/monzo.rs`, `src/notify.rs` — timeout clients; M3 (`Confidence::from_env_str`).
- `src/config.rs` — L5 (manual `Debug`).
- `src/state.rs` — L4 (`MonzoState` manual `Debug`), M8 (`tempfile` persist).
- `src/lock.rs` — M7 (`create_dir_all`).
- `src/main.rs` — H1 top-level timeout; M3 wiring; `mod http;`.
- `Cargo.toml` — promote `tempfile` to dependency.
- `Dockerfile` — M9 (pin base images).

---

## Task 1: H1 — timeouts on every HTTP client + the shim + a top-level backstop

**Files:** Create `src/http.rs`; modify `src/main.rs` (`mod http;` + top-level timeout), `src/claude.rs:153`, `src/banks/monzo.rs:63`, `src/notify.rs:21`, `src/telegram.rs:180`, `src/run.rs:537`, `src/actual/mod.rs:50-70`.

- [ ] **Step 1: Create the shared client builder**

Create `src/http.rs`:

```rust
//! Shared reqwest client builder. `aba` is an unattended one-shot cron job with
//! no external watchdog, so every HTTP client MUST bound connect + total time —
//! a stalled peer would otherwise hang the process and hold the state lock
//! indefinitely.

use std::time::Duration;

/// A client with a 10s connect timeout and the given total-request timeout.
#[must_use]
pub fn client(total: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(total)
        .build()
        // Only fails if the TLS backend can't initialise — a hard startup error.
        .expect("reqwest client builder with timeouts")
}
```

- [ ] **Step 2: Register the module**

In `src/main.rs`, add `mod http;` alongside the other `mod` declarations (near the top, with `mod run;` etc.).

- [ ] **Step 3: Replace the five `reqwest::Client::new()` sites**

Each becomes `crate::http::client(Duration::from_secs(N))` (add `use std::time::Duration;` to each file if not already imported, or use the fully-qualified `std::time::Duration`):

- `src/claude.rs:153` (in `HttpClaude::new`): `http: crate::http::client(std::time::Duration::from_secs(120)),` *(LLM latency)*
- `src/banks/monzo.rs:63`: `http: crate::http::client(std::time::Duration::from_secs(30)),`
- `src/notify.rs:21`: `http: crate::http::client(std::time::Duration::from_secs(15)),`
- `src/telegram.rs:180` (in `HttpTelegram::new`): `http: crate::http::client(std::time::Duration::from_secs(30)),`
- `src/run.rs:537` (`let http = reqwest::Client::new();`): `let http = crate::http::client(std::time::Duration::from_secs(30));`

- [ ] **Step 4: Bound the shim wait + kill on timeout**

In `src/actual/mod.rs`, in `call()`: add `.kill_on_drop(true)` to the command builder so a dropped (timed-out) child is killed, and wrap the wait in a timeout. Change the command-builder chain to include `.kill_on_drop(true)`:

```rust
        cmd.args(&self.script_args)
            .current_dir(&self.cwd)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
```

Replace the wait line (`let output = child.wait_with_output().await.context("shim wait failed")?;`) with:

```rust
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            child.wait_with_output(),
        )
        .await
        {
            Ok(res) => res.context("shim wait failed")?,
            // Future dropped here -> child dropped -> killed via kill_on_drop.
            Err(_) => return Err(anyhow!("shim timed out after 120s")),
        };
```

- [ ] **Step 5: Add a top-level timeout backstop**

In `src/main.rs`, wrap the `run::run(...).await` (Run arm) and `poll(...).await` (Poll arm) in a 600s timeout. For the Run arm, replace `.await` on the `run::run(...)` expression so the arm returns:

```rust
            match tokio::time::timeout(std::time::Duration::from_secs(600), run::run(
                &cfg,
                dry_run,
                &mut state,
                &mut bank,
                &actual,
                &claude,
                &notifier,
                channel
                    .as_ref()
                    .map(|c| c as &dyn crate::telegram::ConfirmChannel),
                autoimport,
            ))
            .await
            {
                Ok(res) => res,
                Err(_) => Err(anyhow::anyhow!("run timed out after 600s")),
            }
```

For the Poll arm, replace `poll(&cfg, &mut state, &channel).await?;` with:

```rust
            match tokio::time::timeout(
                std::time::Duration::from_secs(600),
                poll(&cfg, &mut state, &channel),
            )
            .await
            {
                Ok(res) => res?,
                Err(_) => return Err(anyhow::anyhow!("poll timed out after 600s")),
            }
            Ok(())
```

- [ ] **Step 6: Build, test, clippy**

Run: `cargo build` then `cargo test` then `cargo clippy --all-targets`
Expected: build OK; 78 tests still pass; clippy clean. (No new unit test — timeout wiring is verified by compilation + the suite staying green; reqwest exposes no timeout getters to assert on.)

- [ ] **Step 7: Commit**

```bash
git add src/http.rs src/main.rs src/claude.rs src/banks/monzo.rs src/notify.rs src/telegram.rs src/run.rs src/actual/mod.rs
git commit -m "feat: bound all HTTP clients + shim wait + run/poll with timeouts (H1)"
```

---

## Task 2: C1 (critical) — validate matched payees; hold instead of silently dropping

**Files:** `src/run.rs` (`apply_matches` ~54-80, its call site ~275, the cached-loop arm ~302, the claude-loop arm ~324, imports); test in the `tests` module.

- [ ] **Step 1: Write the failing regression test**

Add to `src/run.rs`'s `tests` module (near the other `run()` integration tests):

```rust
    #[tokio::test]
    async fn high_confidence_match_to_nonexistent_payee_is_held_not_dropped() {
        let shim = tempfile::tempdir().unwrap();
        let statedir = tempfile::tempdir().unwrap();
        let state_path = statedir.path().join("s.toml");
        let imports_log = shim.path().join("imports.log");
        let cfg = cfg_at(&state_path);
        let actual = fake_actual(&shim); // get_payees -> [Local Cafe]
        let notifier = NullNotifier;
        let channel = FakeChannel::new(vec![], 0);

        let mut state = State::default();
        let mut bank = FakeBank {
            txs: vec![tx("tx_ghost", "SQ *GHOST", -999)],
        };
        // Claude returns a HIGH-confidence payee that does NOT exist in Actual.
        let claude = FakeClaude {
            out: vec![PayeeMatch {
                raw: "SQ *GHOST".into(),
                payee: Some("Ghost Payee".into()),
                confidence: Confidence::High,
            }],
        };
        run(
            &cfg, false, &mut state, &mut bank, &actual, &claude, &notifier,
            Some(&channel as &dyn ConfirmChannel), Confidence::High,
        )
        .await
        .unwrap();

        // Not imported...
        assert!(!imports_log.exists(), "non-existent payee must NOT be imported");
        // ...held for review with the proposed name as the suggestion...
        assert_eq!(state.pending_reviews.len(), 1);
        assert_eq!(state.pending_reviews[0].external_id, "tx_ghost");
        assert_eq!(
            state.pending_reviews[0].suggestion.as_deref(),
            Some("Ghost Payee")
        );
        // ...and last_run still advanced (the tx is safely held, not lost).
        assert!(state.last_run.is_some());
    }
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test high_confidence_match_to_nonexistent_payee_is_held_not_dropped`
Expected: FAIL — today the match is dropped (`pending_reviews` is empty; the `warn!` arm at `run.rs:324` fired).

- [ ] **Step 3: Add the existing-payee guard to `apply_matches`**

In `src/run.rs`, add `BTreeSet` to the imports (find the `use std::collections::BTreeMap;` line and make it `use std::collections::{BTreeMap, BTreeSet};`). Change `apply_matches` (lines ~54-80) to take the existing-payee set and route a non-existent payee to `proposed`:

```rust
pub fn apply_matches(
    txs: &[RawTransaction],
    matches: &[PayeeMatch],
    autoimport: Confidence,
    existing: &BTreeSet<&str>,
) -> MatchOutcome {
    let lookup: BTreeMap<&str, &PayeeMatch> = matches.iter().map(|m| (m.raw.as_str(), m)).collect();
    let mut matched = Vec::new();
    let mut proposed = Vec::new();
    let mut unmatched = Vec::new();
    for tx in txs {
        match lookup.get(tx.raw_merchant.as_str()) {
            Some(m) => match &m.payee {
                // Auto-import only a confident match to a payee that ACTUALLY
                // exists in Actual — otherwise it would be silently dropped at
                // import (no payee id), so hold it for review instead.
                Some(payee)
                    if m.confidence.rank() >= autoimport.rank()
                        && existing.contains(payee.as_str()) =>
                {
                    matched.push((tx.external_id.clone(), payee.clone()))
                }
                Some(payee) => proposed.push((tx.clone(), payee.clone(), m.confidence)),
                None => unmatched.push(tx.clone()),
            },
            None => unmatched.push(tx.clone()),
        }
    }
    MatchOutcome { matched, proposed, unmatched }
}
```

- [ ] **Step 4: Update the call site**

In `src/run.rs` (~line 275), build the set and pass it:

```rust
    let existing: BTreeSet<&str> = payee_names.iter().map(String::as_str).collect();
    let outcome = apply_matches(&unknown_txs, &claude_matches, autoimport, &existing);
```

- [ ] **Step 5: Make both silent-drop arms fail loud (hold for review)**

In the cached-hits loop (~302), replace the `(Some(_), None)` arm:

```rust
            (Some(tx), None) => {
                warn!(
                    "cached payee '{payee_name}' for tx {ext_id} no longer exists in Actual; holding for review"
                );
                let date = tx.date.format("%Y-%m-%d").to_string();
                if state.add_pending_full(
                    &tx.external_id, &tx.raw_merchant, tx.amount_minor, &date,
                    Some(payee_name.clone()), None,
                ) {
                    held += 1;
                }
            }
```

In the claude-matches loop (~324), replace the `(Some(_), None)` arm (now defensive — `apply_matches` guarantees `matched` payees exist):

```rust
            (Some(tx), None) => {
                // Defensive: apply_matches guarantees matched payees exist. If
                // this ever fires, hold for review rather than dropping (fail-loud).
                warn!(
                    "matched payee '{payee_name}' for tx {ext_id} unexpectedly missing; holding for review"
                );
                let date = tx.date.format("%Y-%m-%d").to_string();
                if state.add_pending_full(
                    &tx.external_id, &tx.raw_merchant, tx.amount_minor, &date,
                    Some(payee_name.clone()), None,
                ) {
                    held += 1;
                }
            }
```

- [ ] **Step 6: Update other `apply_matches` call sites (tests)**

Run: `cargo test 2>&1 | head -40` (or build) — fix any unit tests that call `apply_matches` with the old 3-arg signature by adding an `existing` set. Find them: `git grep -n "apply_matches(" src/`. For a test passing no real payees, use `&BTreeSet::new()`; for ones expecting an auto-import, include that payee in the set.

- [ ] **Step 7: Run the new test + full suite**

Run: `cargo test high_confidence_match_to_nonexistent_payee_is_held_not_dropped` then `cargo test`
Expected: the new test PASSES; whole suite green. Then `cargo clippy --all-targets` clean.

- [ ] **Step 8: Commit**

```bash
git add src/run.rs
git commit -m "fix(run): hold non-existent matched payees for review instead of dropping (C1)"
```

---

## Task 3: M1 + M4 + M5 + M6 — fail-loud cursor, loud schedule drops, durable message ids

**Files:** `src/run.rs` (fetch cursor ~196-201, `schedules_from_rows` ~89-105, send loop ~443-450); tests in the `tests` module.

- [ ] **Step 1: Write failing tests for the schedule drops (M4/M5)**

Add to `src/run.rs`'s `tests` module (the `ScheduleRow` type and `schedules_from_rows` are in this module's parent — import as needed; mirror how existing schedule tests construct rows, check `git grep -n "schedules_from_rows\|ScheduleRow" src/run.rs`):

```rust
    #[test]
    fn schedules_from_rows_drops_bad_date_and_missing_payee_loudly() {
        let payees = vec![Payee { id: "p1".into(), name: "Rent Co".into() }];
        let rows = vec![
            // good
            ScheduleRow { date: Some("2026-06-01".into()), amount: Some(-1000), payee_id: Some("p1".into()) },
            // bad date -> dropped
            ScheduleRow { date: Some("not-a-date".into()), amount: Some(-1000), payee_id: Some("p1".into()) },
            // missing date -> dropped
            ScheduleRow { date: None, amount: Some(-1000), payee_id: Some("p1".into()) },
            // payee not in list -> dropped
            ScheduleRow { date: Some("2026-06-02".into()), amount: Some(-1000), payee_id: Some("nope".into()) },
        ];
        let schedules = schedules_from_rows(rows, &payees);
        assert_eq!(schedules.len(), 1, "only the fully-valid row survives");
        assert_eq!(schedules[0].payee, "Rent Co");
    }
```

(Adjust `ScheduleRow`/`Payee` field names to the real definitions if they differ — verify with `git grep`.)

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test schedules_from_rows_drops_bad_date_and_missing_payee_loudly`
Expected: FAIL — today the missing-payee row becomes a `""`-payee `Schedule` (len would be 2, not 1).

- [ ] **Step 3: Make `schedules_from_rows` drop loudly (M4/M5)**

In `src/run.rs`, replace the `filter_map` closure body (~89-104):

```rust
    rows.into_iter()
        .filter_map(|r| {
            let Some(date_str) = r.date.as_deref() else {
                warn!("schedule dropped: missing date (payee_id={:?})", r.payee_id);
                return None;
            };
            let date = match NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                Ok(d) => d,
                Err(e) => {
                    warn!("schedule dropped: unparseable date '{date_str}': {e}");
                    return None;
                }
            };
            let Some(payee) = r
                .payee_id
                .as_deref()
                .and_then(|id| by_id.get(id).copied())
            else {
                warn!("schedule dropped: payee_id {:?} not in payee list", r.payee_id);
                return None;
            };
            Some(Schedule {
                amount: r.amount,
                date,
                payee: payee.to_string(),
            })
        })
        .collect()
```

- [ ] **Step 4: Apply the M1 fetch-start cursor**

In `src/run.rs`, find the fetch block (~196-201). Immediately before `let fetched = bank.fetch_since(since).await...`, add:

```rust
    // Cursor for a successful run: stamp BEFORE fetching so a transaction that
    // arrives mid-run is re-fetched next time (dedup covers the overlap) rather
    // than falling into a gap and being lost (M1).
    let fetch_start = Utc::now();
```

Then change the `last_run` advance (~431) from `state.last_run = Some(Utc::now());` to:

```rust
    state.last_run = Some(fetch_start);
```

- [ ] **Step 5: Make message ids durable after each send (M6)**

In `src/run.rs`, in the send loop (~443-450), change the `Ok` arm to persist immediately (best-effort) so a later failure can't strand the id and cause duplicate prompts:

```rust
            match ch.send_prompt(&prompt).await {
                Ok(message_id) => {
                    state.pending_reviews[i].telegram_message_id = Some(message_id);
                    // Persist now so a later failure can't strand this id (M6).
                    if let Err(e) = state.save(&cfg.state_path) {
                        warn!("failed to persist telegram_message_id (may re-prompt next run): {e}");
                    }
                }
                Err(e) => warn!("telegram send_prompt failed (will retry next run): {e}"),
            }
```

(Leave the final `state.save(&cfg.state_path)?;` after the loop — it still persists `last_run` and is the authoritative save.)

- [ ] **Step 6: Run tests + suite**

Run: `cargo test schedules_from_rows_drops_bad_date_and_missing_payee_loudly` then `cargo test` then `cargo clippy --all-targets`
Expected: new test passes; suite green; clippy clean. (M1/M6 are exercised by the existing `run()` tests staying green; the M1 cursor value isn't deterministically unit-testable.)

- [ ] **Step 7: Commit**

```bash
git add src/run.rs
git commit -m "fix(run): fetch-start cursor (M1), loud schedule drops (M4/M5), durable msg ids (M6)"
```

---

## Task 4: H2 + M2 — plain-text Telegram & redacted error bodies

**Files:** `src/telegram.rs` (`build_send_request` ~148-160, three `anyhow!("telegram error ...")` sites ~224/273/306, tests), `src/run.rs` (`build_prompt` chat_text ~127-156, test assertions).

- [ ] **Step 1: Write/adjust the failing tests**

In `src/telegram.rs` tests, add an assertion that no `parse_mode` is emitted — extend `build_send_request_has_text_and_buttons` with:

```rust
        assert!(body.get("parse_mode").is_none(), "prompts must be plain text");
```

Add an M2 redaction test:

```rust
    #[tokio::test]
    async fn telegram_error_body_is_redacted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botSUPERSECRET/sendMessage"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({ "ok": false, "description": "bad", "leaked": "SUPERSECRET" }),
            ))
            .mount(&server)
            .await;
        let tg = HttpTelegram::new("SUPERSECRET".into(), 42).with_base_url(server.uri());
        let prompt = Prompt { chat_text: "hi".into(), buttons: vec![] };
        let err = tg.send_prompt(&prompt).await.expect_err("400 should error");
        let chain = format!("{err:#}");
        assert!(!chain.contains("SUPERSECRET"), "token must be redacted: {chain}");
    }
```

In `src/run.rs` tests, update `build_prompt_picks_flavor_by_payee_membership` to expect plain-text chat_text (e.g. assert it contains `'Local Cafe'` and not `*Local Cafe*`). Verify the exact current assertions with `git grep -n "build_prompt_picks_flavor" src/run.rs` and adjust the expected substrings to the plain-text forms from Step 3/4.

- [ ] **Step 2: Run to confirm failures**

Run: `cargo test telegram_error_body_is_redacted build_send_request_has_text_and_buttons build_prompt_picks_flavor_by_payee_membership`
Expected: FAIL (parse_mode still present; token still leaks; chat_text still has Markdown).

- [ ] **Step 3: Drop `parse_mode` from `build_send_request` (H2)**

In `src/telegram.rs`, the `build_send_request` body currently sets `"parse_mode": "Markdown"`. Remove that line so the JSON is `{ chat_id, text, reply_markup? }`. (After the #7a change the body is built with a base `json!` then conditional `reply_markup`; delete the `"parse_mode": "Markdown",` entry from the base object.)

- [ ] **Step 4: Make `build_prompt` chat_text plain (H2)**

In `src/run.rs` `build_prompt`, replace the three flavor `chat_text` format strings with plain-text equivalents (no `*`/`` ` ``/`_`):

```rust
        // flavor 1 (suggestion is a real payee):
        "'{}' -> '{}'?  ({} confidence)\nReply with a different payee to remap.",
        // (args: entry.raw_merchant, s, conf)

        // flavor 2 (suggestion not a payee):
        "No existing payee '{}'. Create it?\nOr reply with a payee name.",
        // (arg: s)

        // flavor 3 (no suggestion):
        "New merchant '{}' — which payee?\nReply with a payee name, or:",
        // (arg: entry.raw_merchant)
```

- [ ] **Step 5: Redact the three Telegram error bodies (M2)**

In `src/telegram.rs`, each `return Err(anyhow!("telegram error {status}: {value}"));` (in `poll_replies`/getUpdates ~224, `send_prompt`/sendMessage ~273, and `edit_message_text`/editMessageText ~306) becomes:

```rust
            return Err(anyhow!("telegram error {status}: {}", self.redact(value.to_string())));
```

- [ ] **Step 6: Run the tests + suite**

Run: `cargo test` then `cargo clippy --all-targets`
Expected: the three updated tests pass; full suite green; clippy clean.

- [ ] **Step 7: Commit**

```bash
git add src/telegram.rs src/run.rs
git commit -m "fix(telegram): plain-text prompts (H2) and redacted error bodies (M2)"
```

---

## Task 5: H3 + L6 — pipe & redact shim stderr; redact shim error messages

**Files:** `src/actual/mod.rs` (spawn ~50-81), `actual-shim/index.js` (~62-63).

- [ ] **Step 1: Pipe shim stderr and redact it (H3)**

In `src/actual/mod.rs` `call()`, change `.stderr(Stdio::inherit())` to `.stderr(Stdio::piped())`. (The `.kill_on_drop(true)` from Task 1 stays.) After the `let output = ...` wait line, before parsing stdout, add stderr handling:

```rust
        if !output.stderr.is_empty() {
            let mut err_text = String::from_utf8_lossy(&output.stderr).into_owned();
            if !self.password.is_empty() {
                err_text = err_text.replace(&self.password, "***");
            }
            if output.status.success() {
                tracing::debug!("shim stderr: {err_text}");
            } else {
                tracing::error!("shim exited with {:?}: {err_text}", output.status.code());
            }
        }
```

- [ ] **Step 2: Redact the shim error string on the Rust side (L6)**

In the same `call()`, change the error arm `ShimResponse::Err { error } => Err(anyhow!("shim error: {error}")),` to redact the password defensively:

```rust
            ShimResponse::Err { error } => {
                let safe = if self.password.is_empty() {
                    error
                } else {
                    error.replace(&self.password, "***")
                };
                Err(anyhow!("shim error: {safe}"))
            }
```

- [ ] **Step 3: Redact the password in the shim's own error output (L6)**

In `actual-shim/index.js`, add a redact helper and apply it where the error response is built. Replace the `catch` block (lines ~62-63):

```js
  } catch (e) {
    let msg = String((e && e.message) || e);
    if (PASSWORD) msg = msg.split(PASSWORD).join('***');
    response = { ok: false, error: msg };
  } finally {
```

- [ ] **Step 4: Verify**

Run: `cargo build` then `cargo test` then `cargo clippy --all-targets`
Expected: build OK; 78+ tests pass (the existing `actual::tests` shim fakes still work — they don't write to stderr, so the new block is inert in tests); clippy clean.

Run a shim redaction smoke (manual): `node actual-shim/index.js` is awkward to drive without a server; instead confirm by reading that `msg.split(PASSWORD).join('***')` runs only when `PASSWORD` is set. (No automated JS test harness exists in this repo; note this in the commit.)

- [ ] **Step 5: Commit**

```bash
git add src/actual/mod.rs actual-shim/index.js
git commit -m "fix(actual): pipe+redact shim stderr (H3) and redact shim error messages (L6)"
```

---

## Task 6: M3 — strict `ABA_AUTOIMPORT_CONFIDENCE` parsing

**Files:** `src/claude.rs` (`Confidence` impl ~22-40, test), `src/main.rs:93`.

- [ ] **Step 1: Write the failing test**

In `src/claude.rs` tests module:

```rust
    #[test]
    fn from_env_str_is_strict() {
        assert_eq!(Confidence::from_env_str("high").unwrap(), Confidence::High);
        assert_eq!(Confidence::from_env_str("medium").unwrap(), Confidence::Medium);
        assert_eq!(Confidence::from_env_str("low").unwrap(), Confidence::Low);
        assert!(Confidence::from_env_str("High").is_err(), "case-typo must error");
        assert!(Confidence::from_env_str("bogus").is_err());
    }
```

- [ ] **Step 2: Run to confirm it fails**

Run: `cargo test from_env_str_is_strict`
Expected: FAIL — `from_env_str` doesn't exist.

- [ ] **Step 3: Add the strict parser**

In `src/claude.rs`, inside `impl Confidence` (after `parse`):

```rust
    /// Strict parse for the operator-set env var. Unlike `parse` (which leniently
    /// defaults unknown Claude API output to Medium), a bad value here is a hard
    /// config error — a typo like "High"/"MEDIUM" must not silently widen
    /// auto-import.
    pub fn from_env_str(s: &str) -> anyhow::Result<Confidence> {
        match s {
            "high" => Ok(Confidence::High),
            "medium" => Ok(Confidence::Medium),
            "low" => Ok(Confidence::Low),
            other => Err(anyhow::anyhow!(
                "ABA_AUTOIMPORT_CONFIDENCE must be high|medium|low, got '{other}'"
            )),
        }
    }
```

- [ ] **Step 4: Use it on the env path**

In `src/main.rs:93`, change `let autoimport = Confidence::parse(&cfg.autoimport_confidence);` to:

```rust
            let autoimport = Confidence::from_env_str(&cfg.autoimport_confidence)?;
```

- [ ] **Step 5: Run tests + suite**

Run: `cargo test from_env_str_is_strict` then `cargo test` then `cargo clippy --all-targets`
Expected: new test passes; suite green; clippy clean.

- [ ] **Step 6: Commit**

```bash
git add src/claude.rs src/main.rs
git commit -m "fix(config): strict ABA_AUTOIMPORT_CONFIDENCE parsing, error on typo (M3)"
```

---

## Task 7: L4 + L5 — redacting `Debug` for secrets

**Files:** `src/state.rs` (`MonzoState` ~23-29), `src/config.rs` (`Config` ~41-60); tests.

- [ ] **Step 1: Write the failing tests**

In `src/state.rs` tests:

```rust
    #[test]
    fn monzo_state_debug_redacts_refresh_token() {
        let m = MonzoState { refresh_token: "SECRET_RT".into(), account_id: "acc_1".into() };
        let s = format!("{m:?}");
        assert!(!s.contains("SECRET_RT"), "refresh_token must be redacted: {s}");
        assert!(s.contains("acc_1"), "account_id is fine to show");
    }
```

In `src/config.rs` tests (there is already a `from_getter` test helper producing a `Config`; reuse it):

```rust
    #[test]
    fn config_debug_redacts_secrets() {
        let cfg = Config::from_getter(|k| match k {
            "ANTHROPIC_API_KEY" => Some("AK_SECRET".into()),
            "MONZO_CLIENT_ID" => Some("cid".into()),
            "MONZO_CLIENT_SECRET" => Some("CS_SECRET".into()),
            "MONZO_REDIRECT_URI" => Some("http://localhost/cb".into()),
            "APPRISE_URL" => Some("tgram://APP_SECRET/123".into()),
            "ACTUAL_SERVER_URL" => Some("http://actual".into()),
            "ACTUAL_PASSWORD" => Some("PW_SECRET".into()),
            "ACTUAL_SYNC_ID" => Some("sync".into()),
            "ABA_STATE_PATH" => Some("/tmp/s.toml".into()),
            "TELEGRAM_BOT_TOKEN" => Some("BOT_SECRET".into()),
            _ => None,
        })
        .unwrap();
        let s = format!("{cfg:?}");
        for secret in ["AK_SECRET", "CS_SECRET", "PW_SECRET", "APP_SECRET", "BOT_SECRET"] {
            assert!(!s.contains(secret), "secret {secret} leaked in Debug: {s}");
        }
        assert!(s.contains("cid"), "non-secret client id should show");
    }
```

(If `Config::from_getter`'s required keys differ, mirror the existing `cfg_at`/`from_getter` test in this file.)

- [ ] **Step 2: Run to confirm they fail**

Run: `cargo test monzo_state_debug_redacts_refresh_token config_debug_redacts_secrets`
Expected: FAIL — derived `Debug` prints the secrets verbatim.

- [ ] **Step 3: Hand-write `MonzoState` Debug (L4)**

In `src/state.rs`, remove `Debug` from `MonzoState`'s derive (`#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]` → `#[derive(Default, PartialEq, Serialize, Deserialize)]`) and add:

```rust
impl std::fmt::Debug for MonzoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonzoState")
            .field(
                "refresh_token",
                &if self.refresh_token.is_empty() { "<empty>" } else { "***" },
            )
            .field("account_id", &self.account_id)
            .finish()
    }
}
```

(`State` keeps its derived `Debug` — it now transitively uses this redacted impl, and holds no other secret.)

- [ ] **Step 4: Hand-write `Config` Debug (L5)**

In `src/config.rs`, remove `Debug` from `Config`'s derive (`#[derive(Debug, Clone)]` → `#[derive(Clone)]`) and add:

```rust
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("anthropic_api_key", &"***")
            .field("claude_model", &self.claude_model)
            .field("monzo_client_id", &self.monzo_client_id)
            .field("monzo_client_secret", &"***")
            .field("monzo_redirect_uri", &self.monzo_redirect_uri)
            .field("apprise_url", &"***")
            .field("actual_server_url", &self.actual_server_url)
            .field("actual_password", &"***")
            .field("actual_sync_id", &self.actual_sync_id)
            .field("state_path", &self.state_path)
            .field("shim_dir", &self.shim_dir)
            .field("monzo_base_url", &self.monzo_base_url)
            .field("telegram_bot_token", &self.telegram_bot_token.as_ref().map(|_| "***"))
            .field("telegram_chat_id", &self.telegram_chat_id)
            .field("autoimport_confidence", &self.autoimport_confidence)
            .field("lock_path", &self.lock_path)
            .field("telegram_base_url", &self.telegram_base_url)
            .finish()
    }
}
```

- [ ] **Step 5: Run tests + suite**

Run: `cargo test monzo_state_debug_redacts_refresh_token config_debug_redacts_secrets` then `cargo test` then `cargo clippy --all-targets`
Expected: new tests pass; suite green; clippy clean. (If any other test relied on `Config`/`MonzoState` `Debug` output format, it will surface here — fix to the redacted form.)

- [ ] **Step 6: Commit**

```bash
git add src/state.rs src/config.rs
git commit -m "fix: redacting Debug impls for Config and MonzoState (L4/L5)"
```

---

## Task 8: M7 + M8 + M9 — lock dir, robust atomic save, pinned Docker images

**Files:** `src/lock.rs`, `src/state.rs` (`save` ~74-86), `Cargo.toml`, `Dockerfile`; test in `src/lock.rs`.

- [ ] **Step 1: Write the failing lock test (M7)**

In `src/lock.rs` tests:

```rust
    #[test]
    fn lock_creates_missing_parent_dir() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b/c/aba.lock"); // parents don't exist yet
        let held = lock_blocking(&nested).unwrap();
        assert!(nested.exists());
        drop(held);
    }
```

- [ ] **Step 2: Run to confirm it fails**

Run: `cargo test lock_creates_missing_parent_dir`
Expected: FAIL — `File::create` errors `NotFound` because `a/b/c` doesn't exist.

- [ ] **Step 3: Create the lock parent dir (M7)**

In `src/lock.rs`, add a small helper and call it at the top of both `lock_blocking` and `lock_try`, before `File::create`:

```rust
fn ensure_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
```

Then in each function, before `let f = File::create(path)?;` add `ensure_parent(path)?;`.

- [ ] **Step 4: Promote `tempfile` to a dependency (M8)**

In `Cargo.toml`, move `tempfile` from `[dev-dependencies]` to `[dependencies]` (keep the same version). If it's only under dev-deps, add it under `[dependencies]` and remove the dev-deps line (or leave it in both — but a single `[dependencies]` entry suffices for both build and test).

- [ ] **Step 5: Use `NamedTempFile::persist` for the atomic save (M8)**

In `src/state.rs`, replace the body of `save` (~74-86):

```rust
    /// Atomic write: write to a temp file in the target directory, then persist
    /// (rename) over the destination. Same-dir temp avoids cross-filesystem
    /// EXDEV rename failures.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write as _;
        let body = toml::to_string_pretty(self)?;
        let dir = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => {
                std::fs::create_dir_all(p)?;
                p.to_path_buf()
            }
            None => std::path::PathBuf::from("."),
        };
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        tmp.write_all(body.as_bytes())?;
        tmp.flush()?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
```

- [ ] **Step 6: Pin Docker base images (M9)**

Attempt to resolve digests: run `docker manifest inspect rust:1-slim` and `docker manifest inspect node:22-slim` (or `docker buildx imagetools inspect`). 
- If digests are obtained, pin each `FROM` in `Dockerfile` as `FROM rust:1-slim@sha256:<digest> AS rust-build`, `FROM node:22-slim@sha256:<digest> AS node-deps`, and the same node digest for the `runtime` stage.
- If Docker/registry is unreachable, pin to specific patch tags instead (e.g. find the current via `docker images`/Docker Hub and use `FROM node:22.x.y-slim`), and add `# TODO: pin @sha256 digest at deploy` above each `FROM`.

Document which path you took in the commit body.

- [ ] **Step 7: Run tests + build**

Run: `cargo test lock_creates_missing_parent_dir` then `cargo test` then `cargo clippy --all-targets`
Expected: new test passes; full suite green (the existing `state::tests` save/round-trip tests must still pass with the `tempfile` implementation); clippy clean.
If Docker is available: `docker build -t aba:audit-check .` succeeds with the pinned images.

- [ ] **Step 8: Commit**

```bash
git add src/lock.rs src/state.rs Cargo.toml Cargo.lock Dockerfile
git commit -m "fix: lock-dir creation (M7), tempfile persist save (M8), pinned Docker images (M9)"
```

---

## Self-Review

**Spec coverage (16 items):**
- C1 → Task 2 ✅ · H1 → Task 1 ✅ · H2 → Task 4 ✅ · H3 → Task 5 ✅
- M1 → Task 3 ✅ · M2 → Task 4 ✅ · M3 → Task 6 ✅ · M4 → Task 3 ✅ · M5 → Task 3 ✅ · M6 → Task 3 ✅ · M7 → Task 8 ✅ · M8 → Task 8 ✅ · M9 → Task 8 ✅
- L4 → Task 7 ✅ · L5 → Task 7 ✅ · L6 → Task 5 ✅
- 3 Phase-0 regression tests: C1-held (Task 2), Markdown-special/plain prompt (Task 4), schedule loud-drop (Task 3); the "last_run==None after import failure" assertion the audit flagged is already covered by existing tests (audit §5) — add only if `git grep` shows it's missing.

**Placeholder scan:** No TBD/TODO except the intentional Docker `# TODO: pin @sha256 at deploy` fallback (M9). Every code step has concrete code.

**Type/signature consistency:** `apply_matches(txs, matches, autoimport, existing: &BTreeSet<&str>)` — the new 4th arg is added at the def (Task 2 Step 3), the prod call site (Step 4), and all test call sites (Step 6). `Confidence::from_env_str(&str) -> anyhow::Result<Confidence>` defined in Task 6 Step 3, used in Task 6 Step 4. `crate::http::client(Duration) -> reqwest::Client` defined Task 1 Step 1, used at 5 sites Step 3. `MonzoState`/`Config` lose `derive(Debug)` and gain manual impls (Task 7) — `State` keeps derived `Debug` (transitively redacted). `tempfile` promoted to a dep (Task 8 Step 4) before `state.rs` uses it at runtime (Step 5).

**Cross-task notes:** Tasks are independent per file/concern and each ends with a green suite, so they can run in any order; the natural order is 1→8. Task 5 depends on Task 1 only for the `.kill_on_drop(true)` line already being present (Task 5 edits the adjacent `.stderr(...)`); if Task 5 runs before Task 1, add `.kill_on_drop(true)` there instead. Verify everything with `cargo test` + `cargo clippy --all-targets` (not IDE diagnostics).
