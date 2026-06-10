# Review Remediation Phase A Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every small/self-contained finding from the 2026-06-09 deep-dive review: deferred 2026-06-02 audit items, new observations, observability, and the three approved extras.

**Architecture:** 14 independent tasks against the existing one-shot CLI; no module restructuring (that is Phase B). Each task is a separate commit that leaves `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` green.

**Tech Stack:** Rust edition 2024 (clippy pedantic+nursery via `[lints.clippy]`), tokio, reqwest, serde/toml, tiny_http (dev OAuth only), Node shim (`actual-shim/index.js`, `@actual-app/api`), GitHub Actions.

**Follow-up phases (planned separately, in order, after this lands):**
- Phase B: `run.rs` decomposition (`pipeline/`, `telegram/prompts.rs`, `report.rs`)
- Phase C: ID newtypes (`MonzoAccountId`, `MonzoPotId`, `ActualAccountId`, `ExternalTxId`)
- Phase D: poll-side prompt chaining (cached payee/category name lists in state; poll sends the category prompt immediately after a payee reply)

**Verification gate (every task, run in this order; never infer success from a piped exit code — grep output for `error[`/`FAILED`):**
```bash
cargo fmt --all
cargo clippy --all-targets          # warn pass (matches edit hook)
cargo clippy --all-targets -- -D warnings
cargo test
cargo fmt --all -- --check
```

---

### Task 1: `rust-version` floor in Cargo.toml

**Files:**
- Modify: `Cargo.toml` (the `[package]` table, lines 1-4)

- [ ] **Step 1: Add the floor**

In `Cargo.toml`, change:
```toml
[package]
name = "actual-budget-automation"
version = "0.1.0"
edition = "2024"
```
to:
```toml
[package]
name = "actual-budget-automation"
version = "0.1.0"
edition = "2024"
# File::lock() (src/lock.rs) stabilised in 1.89 — older toolchains fail with a
# cryptic "method not found" without this floor.
rust-version = "1.89"
```

- [ ] **Step 2: Run the verification gate**

Run the standard gate. Expected: all green (this is metadata only; `cargo build` additionally verifies the local toolchain satisfies the floor).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml
git commit -m "chore: declare rust-version 1.89 floor (File::lock MSRV)"
```

---

### Task 2: Pin `@actual-app/api` exactly

**Files:**
- Modify: `actual-shim/package.json:8`
- Modify: `actual-shim/package-lock.json` (regenerated, not hand-edited)

- [ ] **Step 1: Pin the version**

In `actual-shim/package.json`, change:
```json
  "dependencies": {
    "@actual-app/api": "^26.0.0"
  }
```
to:
```json
  "dependencies": {
    "@actual-app/api": "26.5.2"
  }
```
(26.5.2 is the version already resolved in `package-lock.json:18` — this changes the *allowed range*, not the installed code.)

- [ ] **Step 2: Sync the lockfile**

```bash
cd actual-shim && npm install --package-lock-only
```
Expected: `package-lock.json` updated in place; `git diff actual-shim/package-lock.json` shows only the root dependency spec changing from `^26.0.0` to `26.5.2` (no resolved-version changes).

- [ ] **Step 3: Verify npm ci still works against the lock**

```bash
cd actual-shim && npm ci --omit=dev --dry-run
```
Expected: completes without `EUSAGE`/lock-mismatch errors.

- [ ] **Step 4: Commit**

```bash
git add actual-shim/package.json actual-shim/package-lock.json
git commit -m "chore(shim): pin @actual-app/api to exact 26.5.2"
```

---

### Task 3: `cargo audit` + `npm audit` in CI

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Add an `audit` job and gate the image on it**

In `.github/workflows/release.yml`, after the `test` job (line 21) insert:
```yaml
  audit:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: taiki-e/install-action@cargo-audit
      - run: cargo audit
      - uses: actions/setup-node@v4
        with:
          node-version: 22
      - run: npm audit --omit=dev --audit-level=high
        working-directory: actual-shim
```
and change the `image` job dependency (line 24) from:
```yaml
  image:
    needs: test
```
to:
```yaml
  image:
    needs: [test, audit]
```

- [ ] **Step 2: Validate the workflow file parses**

```bash
gh workflow list 2>$null; git diff --stat .github/workflows/release.yml
```
(Local YAML sanity: the diff shows only the new job + the `needs` line. Full validation happens on push; do not push yet.)

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: gate image build on cargo audit + npm audit (supply-chain)"
```

---

### Task 4: Fix stale p2p comment in `monzo.rs`

**Files:**
- Modify: `src/banks/monzo.rs:69-70`

- [ ] **Step 1: Replace the comment**

Change:
```rust
impl MonzoTx {
    /// Classify this row as a transfer, if it is one. Pots only in this slice;
    /// inter-account (`p2p_payment`) is added in the inter-account slice.
    fn transfer_target(&self) -> Option<TransferTarget> {
```
to:
```rust
impl MonzoTx {
    /// Classify this row as a transfer, if it is one. Pots here; `monzo_flex`
    /// lines are classified separately (Flex flow). `p2p_payment` is
    /// deliberately NOT a transfer target: friend payments must remain ordinary
    /// transactions or real income/expenses would be suppressed as transfers.
    fn transfer_target(&self) -> Option<TransferTarget> {
```
(The absence of `p2p_payment` is load-bearing — commit 3e47ed4's friend-payment fix depends on it; the comment now states that instead of pointing at a superseded plan.)

- [ ] **Step 2: Run the verification gate, then commit**

```bash
git add src/banks/monzo.rs
git commit -m "docs(monzo): document that p2p_payment is intentionally not a transfer target"
```

---

### Task 5: Trim Anthropic error body to `error.message`

**Files:**
- Modify: `src/claude.rs:233-235`
- Test: `src/claude.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/claude.rs` (wiremock is already a dev-dependency; mirror the existing async-test style used in `src/banks/monzo.rs`):
```rust
#[tokio::test]
async fn classify_error_uses_message_not_full_body() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
            "type": "error",
            "error": { "type": "rate_limit_error", "message": "Rate limited." },
            "internal_debug_blob": "DO-NOT-LOG-ME"
        })))
        .mount(&server)
        .await;
    let c = HttpClaude::new("k".into(), "m".into()).with_base_url(server.uri());
    let err = c.classify(&["X".into()], &[], &[]).await.unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("429"), "status retained: {s}");
    assert!(s.contains("Rate limited."), "message retained: {s}");
    assert!(!s.contains("DO-NOT-LOG-ME"), "full body must not leak: {s}");
}
```

- [ ] **Step 2: Run it, verify it fails**

```bash
cargo test classify_error_uses_message_not_full_body
```
Expected: FAIL on the `DO-NOT-LOG-ME` assertion (current code interpolates the whole body).

- [ ] **Step 3: Implement**

In `src/claude.rs`, change:
```rust
        if !status.is_success() {
            return Err(anyhow!("anthropic error {status}: {value}"));
        }
```
to:
```rust
        if !status.is_success() {
            // Surface only the API's message, not the whole body (log hygiene).
            let msg = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(anyhow!("anthropic error {status}: {msg}"));
        }
```

- [ ] **Step 4: Run the full gate (test now passes), then commit**

```bash
git add src/claude.rs
git commit -m "fix(claude): surface only error.message from Anthropic error bodies"
```

---

### Task 6: `0o700` on a state directory we create

**Files:**
- Modify: `src/state.rs:255-261` (`State::save`)
- Test: `src/state.rs` tests module

- [ ] **Step 1: Write the failing test (unix-only)**

Add to `mod tests` in `src/state.rs`:
```rust
#[cfg(unix)]
#[test]
fn save_creates_private_state_dir() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("newly").join("created");
    let path = nested.join("s.toml");
    State::default().save(&path).unwrap();
    let mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "state dir we create must be owner-only");
}
```
NOTE: this test cannot run on the Windows dev box (`#[cfg(unix)]` keeps it compiled out); it executes in CI (ubuntu). Locally, verify compilation only.

- [ ] **Step 2: Implement**

In `State::save`, change:
```rust
        let dir = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => {
                std::fs::create_dir_all(p)?;
                p.to_path_buf()
            }
            None => std::path::PathBuf::from("."),
        };
```
to:
```rust
        let dir = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => {
                // Only tighten permissions on a directory WE create — never
                // chmod a pre-existing dir the operator set up (e.g. /data).
                let created = !p.exists();
                std::fs::create_dir_all(p)?;
                #[cfg(unix)]
                if created {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
                }
                #[cfg(not(unix))]
                let _ = created;
                p.to_path_buf()
            }
            None => std::path::PathBuf::from("."),
        };
```
(`created` checks only the leaf; `create_dir_all` may create parents too — leaf-only 0o700 is sufficient since the state file lives in the leaf. Keep it simple.)

- [ ] **Step 3: Run the full gate (Windows: test is cfg'd out, everything must still compile), then commit**

```bash
git add src/state.rs
git commit -m "feat(state): chmod 0700 on state dirs created by save (unix)"
```

---

### Task 7: OAuth listener timeout

**Files:**
- Modify: `src/run.rs:1396` (`auth_monzo`)

- [ ] **Step 1: Implement**

In `auth_monzo`, change:
```rust
    let request = server.recv().context("listener error")?;
```
to:
```rust
    // Bounded wait: a browser that never redirects must not hang the command
    // forever. 300s is generous for the Monzo consent screen.
    let request = server
        .recv_timeout(std::time::Duration::from_secs(300))
        .context("listener error")?
        .ok_or_else(|| {
            anyhow::anyhow!("timed out after 300s waiting for the Monzo redirect; re-run `aba auth monzo`")
        })?;
```
(`tiny_http::Server::recv_timeout` returns `io::Result<Option<Request>>` — `None` is the timeout case.)

- [ ] **Step 2: Run the full gate, then commit**

This is a dev-only interactive path — no unit test (would require a real socket race); the type system + existing OAuth tests (`verify_state`, `random_state`) cover the rest.
```bash
git add src/run.rs
git commit -m "fix(auth): bound the OAuth redirect listener wait at 300s"
```

---

### Task 8: Telegram 4096-char truncation guard

**Files:**
- Modify: `src/telegram.rs` (send/edit request building)
- Test: `src/telegram.rs` tests module

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/telegram.rs`:
```rust
#[test]
fn truncate_caps_at_telegram_limit_on_char_boundary() {
    // 5000 multibyte chars (é is 2 bytes in UTF-8) — must cap by BYTES without
    // splitting a char, and end with an ellipsis.
    let long: String = "é".repeat(5000);
    let t = truncate_for_telegram(&long);
    assert!(t.len() <= TELEGRAM_TEXT_LIMIT, "byte len {} > limit", t.len());
    assert!(t.ends_with('…'), "must signal truncation");
    assert!(t.is_char_boundary(t.len() - '…'.len_utf8()));
}

#[test]
fn truncate_leaves_short_text_untouched() {
    assert_eq!(truncate_for_telegram("hello"), "hello");
}
```

- [ ] **Step 2: Run them, verify they fail to compile (function/const missing)**

```bash
cargo test truncate_
```
Expected: compile error — `truncate_for_telegram` not found.

- [ ] **Step 3: Implement**

Add to `src/telegram.rs` (module level, near the other helpers):
```rust
/// Telegram rejects message text over 4096 chars with HTTP 400 — which would
/// re-fail identically every run (same failure shape as the old Markdown bug).
pub(crate) const TELEGRAM_TEXT_LIMIT: usize = 4096;

/// Cap text at the Telegram limit on a char boundary, appending `…` when cut.
pub(crate) fn truncate_for_telegram(text: &str) -> String {
    if text.len() <= TELEGRAM_TEXT_LIMIT {
        return text.to_string();
    }
    let budget = TELEGRAM_TEXT_LIMIT - '…'.len_utf8();
    let cut = (0..=budget).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    let mut out = text[..cut].to_string();
    out.push('…');
    out
}
```
Then apply it where outgoing text enters the request JSON — in the function that builds the sendMessage body (`build_send_request`, around `src/telegram.rs:178-192`) wrap the text field: `"text": truncate_for_telegram(&prompt.chat_text)`, and in `edit_message_text`'s body likewise wrap the `text` value.

- [ ] **Step 4: Run the full gate, then commit**

```bash
git add src/telegram.rs
git commit -m "fix(telegram): truncate outgoing text at the 4096-char API limit"
```

---

### Task 9: `ShimResponse` flat struct with explicit `ok` discriminant

**Files:**
- Modify: `src/actual/shim_protocol.rs:68-73`
- Modify: `src/actual/mod.rs:90-107`
- Test: `src/actual/shim_protocol.rs` tests module

- [ ] **Step 1: Rewrite the response tests to pin the new contract (failing first)**

In `src/actual/shim_protocol.rs` replace the `response_deserialises_ok_and_error` test with:
```rust
#[test]
fn response_parses_ok_and_error_via_discriminant() {
    let ok: ShimResponse<Vec<Payee>> = serde_json::from_value(serde_json::json!({
        "ok": true, "data": [{ "id": "p1", "name": "Amazon" }]
    }))
    .unwrap();
    assert!(ok.ok);
    assert_eq!(ok.data.unwrap().len(), 1);
    assert_eq!(ok.error, None);

    let err: ShimResponse<Vec<Payee>> = serde_json::from_value(serde_json::json!({
        "ok": false, "error": "boom"
    }))
    .unwrap();
    assert!(!err.ok);
    assert!(err.data.is_none());
    assert_eq!(err.error.as_deref(), Some("boom"));
}

#[test]
fn response_ok_true_without_data_is_parseable_but_dataless() {
    // The mod.rs caller turns this into an explicit error instead of a panic.
    let r: ShimResponse<Vec<Payee>> =
        serde_json::from_value(serde_json::json!({ "ok": true })).unwrap();
    assert!(r.ok);
    assert!(r.data.is_none());
}
```

- [ ] **Step 2: Run, verify compile failure (enum has no fields)**

```bash
cargo test response_
```

- [ ] **Step 3: Implement the flat struct**

In `src/actual/shim_protocol.rs` replace:
```rust
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ShimResponse<T> {
    Ok { data: T },
    Err { error: String },
}
```
with:
```rust
/// Flat shim envelope. The shim always emits `ok`; `data` xor `error` is
/// populated. An explicit discriminant (not `#[serde(untagged)]`) so a future
/// shim change can never silently misparse into the wrong variant.
#[derive(Debug, Deserialize)]
pub struct ShimResponse<T> {
    pub ok: bool,
    #[serde(default)]
    pub data: Option<T>,
    #[serde(default)]
    pub error: Option<String>,
}
```
(`#[serde(default)]` on an `Option` field defaults to `None` without requiring `T: Default`.)

In `src/actual/mod.rs` replace the match (lines 97-107):
```rust
        match parsed {
            ShimResponse::Ok { data } => Ok(data),
            ShimResponse::Err { error } => {
                let safe = if self.password.is_empty() {
                    error
                } else {
                    error.replace(&self.password, "***")
                };
                Err(anyhow!("shim error: {safe}"))
            }
        }
```
with:
```rust
        if parsed.ok {
            parsed
                .data
                .ok_or_else(|| anyhow!("shim returned ok=true but no data field"))
        } else {
            let error = parsed
                .error
                .unwrap_or_else(|| "shim returned ok=false with no error message".to_string());
            let safe = if self.password.is_empty() {
                error
            } else {
                error.replace(&self.password, "***")
            };
            Err(anyhow!("shim error: {safe}"))
        }
```

- [ ] **Step 4: Run the full gate (the `actual/mod.rs` subprocess tests exercise the real shim envelope end-to-end), then commit**

```bash
git add src/actual/shim_protocol.rs src/actual/mod.rs
git commit -m "refactor(shim): flat ShimResponse with explicit ok discriminant"
```

---

### Task 10: State schema versioning

**Files:**
- Modify: `src/state.rs` (`State` struct lines 9-27, `load` lines 222-234)
- Test: `src/state.rs` tests module

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/state.rs`:
```rust
#[test]
fn legacy_state_without_version_loads_as_v1() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.toml");
    std::fs::write(&p, "last_run = \"2026-06-01T00:00:00Z\"\n").unwrap();
    let s = State::load(&p).unwrap();
    assert_eq!(s.version, CURRENT_STATE_VERSION);
}

#[test]
fn newer_state_version_refuses_to_load() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.toml");
    std::fs::write(&p, format!("version = {}\n", CURRENT_STATE_VERSION + 1)).unwrap();
    let err = State::load(&p).unwrap_err().to_string();
    assert!(err.contains("newer"), "must explain downgrade refusal: {err}");
}

#[test]
fn fresh_default_state_round_trips_with_current_version() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s.toml");
    State::default().save(&p).unwrap();
    let s = State::load(&p).unwrap();
    assert_eq!(s.version, CURRENT_STATE_VERSION);
}
```

- [ ] **Step 2: Run, verify compile failure (`version` field missing)**

```bash
cargo test -- state_version legacy_state newer_state fresh_default
```

- [ ] **Step 3: Implement**

In `src/state.rs`:

1. Add above the struct:
```rust
/// Bump when the TOML schema changes shape incompatibly. `load` refuses files
/// from a NEWER binary (rollback safety); older files are migrated forward.
pub const CURRENT_STATE_VERSION: u32 = 1;

const fn default_state_version() -> u32 {
    // Absent field = pre-versioning file = schema v1.
    CURRENT_STATE_VERSION
}
```

2. Remove `Default` from the `State` derive and add the field:
```rust
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Schema version; see [`CURRENT_STATE_VERSION`].
    #[serde(default = "default_state_version")]
    pub version: u32,
    pub last_run: Option<DateTime<Utc>>,
    ...remaining fields unchanged...
}
```

3. Add a manual `Default` (derive would set `version: 0`):
```rust
impl Default for State {
    fn default() -> Self {
        Self {
            version: CURRENT_STATE_VERSION,
            last_run: None,
            monzo: MonzoState::default(),
            payee_cache: BTreeMap::new(),
            category_cache: BTreeMap::new(),
            pending_reviews: Vec::new(),
            held_transfers: Vec::new(),
            telegram: TelegramState::default(),
        }
    }
}
```

4. In `State::load`, after `toml::from_str` succeeds and before `migrate_legacy_account`:
```rust
                if state.version > CURRENT_STATE_VERSION {
                    anyhow::bail!(
                        "state file {} is schema v{} — newer than this binary supports (v{}); \
                         refusing to load to avoid silently dropping fields on the next save",
                        path.display(),
                        state.version,
                        CURRENT_STATE_VERSION
                    );
                }
                state.version = CURRENT_STATE_VERSION;
```

- [ ] **Step 4: Run the full gate (the many existing `State::default()` test call sites must all still compile), then commit**

```bash
git add src/state.rs
git commit -m "feat(state): schema version field with refuse-newer rollback guard"
```

---

### Task 11: Structured run summary + meaningful exit codes

**Files:**
- Modify: `src/run.rs` (top of `run()` ~line 743; summary block lines 1304-1346)
- Modify: `src/main.rs` (main signature + dispatch)

- [ ] **Step 1: Add timing + one structured summary line in `run()`**

At the top of `run()` (first statement, before the auth match):
```rust
    let started = std::time::Instant::now();
```
Then replace the current summary log (lines 1304-1307):
```rust
    info!(
        "imported added={} updated={} held={held} flex_mirrors_suppressed={flex_mirrors_suppressed}",
        summary.added, summary.updated
    );
```
by moving the `awaiting` computation (currently at lines 1318-1323) up to just before it, and emitting structured fields:
```rust
    let awaiting = state
        .pending_reviews
        .iter()
        .map(|p| p.raw_merchant.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    info!(
        fetched = fetched_count,
        added = summary.added,
        updated = summary.updated,
        held,
        transfers_imported,
        flex_mirrors_suppressed,
        awaiting_merchants = awaiting,
        held_transfers = state.held_transfers.len(),
        duration_secs = started.elapsed().as_secs(),
        "run complete"
    );
```
(Delete the now-duplicate `awaiting` computation further down; the notification body below it keeps using the moved variable.)

- [ ] **Step 2: Exit codes in `main.rs`**

Contract: `0` success (including the deliberate clean exit on Monzo auth-expired, which already notifies via Apprise); `1` runtime failure (transient — cron retries tomorrow); `2` configuration error (needs a human; retrying cannot help).

Rewrite `main` in `src/main.rs`:
```rust
#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    // Config problems are operator errors: exit 2 so cron alerting can
    // distinguish "fix your env" from a transient runtime failure (exit 1).
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("configuration error: {e:#}");
            return std::process::ExitCode::from(2);
        }
    };
    match dispatch(cli, &cfg).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli, cfg: &Config) -> Result<()> {
    match cli.command {
        // ...the existing match body of main, unchanged, with `cfg` taken by
        // reference (`&cfg` becomes `cfg`)...
    }
}
```
Also move the `Confidence::from_env_str(&cfg.autoimport_confidence)` validation (currently inside the `Run` arm at line 106) up into `main` right after `Config::from_env()` succeeds, so a confidence typo is also exit 2:
```rust
    if let Err(e) = Confidence::from_env_str(&cfg.autoimport_confidence) {
        tracing::error!("configuration error: {e:#}");
        return std::process::ExitCode::from(2);
    }
```
(The `Run` arm keeps its call — it returns the already-validated value — or receives it as a parameter; either is fine, keep the diff minimal.)

- [ ] **Step 3: Run the full gate, then commit**

```bash
git add src/main.rs src/run.rs
git commit -m "feat(observability): structured run-complete summary; exit 2 for config errors"
```

---

### Task 12: `aba check-config` subcommand

**Files:**
- Modify: `src/config.rs:19-41` (`Command` enum)
- Modify: `src/main.rs` (dispatch)

- [ ] **Step 1: Add the subcommand**

In `src/config.rs`, add a variant to `Command`:
```rust
    /// Validate environment configuration and exit (deploy smoke test).
    CheckConfig,
}
```

- [ ] **Step 2: Handle it in `dispatch`**

In `src/main.rs` add an arm:
```rust
        Command::CheckConfig => {
            // Config::from_env() already succeeded before dispatch; validate
            // the lazily-checked knobs that would otherwise fail mid-run.
            Confidence::from_env_str(&cfg.autoimport_confidence)?;
            match (&cfg.telegram_bot_token, &cfg.telegram_chat_id) {
                (Some(_), Some(chat)) => {
                    chat.parse::<i64>().map_err(|_| {
                        anyhow::anyhow!("TELEGRAM_CHAT_ID is not a valid integer: {chat}")
                    })?;
                }
                (None, None) => {}
                _ => anyhow::bail!(
                    "TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be set together (or neither)"
                ),
            }
            println!("config OK");
            Ok(())
        }
```
Note this is intentionally STRICTER than `build_channel` (which only warns and disables Telegram): check-config exists to surface typos loudly. Exit code falls out of Task 11: invalid → 1 from dispatch, missing env → 2.

- [ ] **Step 3: Manual verification**

```bash
cargo run -- check-config            # with the dev env file loaded
```
Expected: `config OK` and exit 0; with `ABA_AUTOIMPORT_CONFIDENCE=HIGHH` expected non-zero exit and a clear message.

- [ ] **Step 4: Run the full gate, then commit**

```bash
git add src/config.rs src/main.rs
git commit -m "feat(cli): check-config subcommand for deploy smoke tests"
```

---

### Task 13: Monzo HTTP status helper (dedup 4 blocks)

**Files:**
- Modify: `src/banks/monzo.rs` (sites at ~224-234, 298-303, 330-333, 381-392)

- [ ] **Step 1: Add the helper**

In `src/banks/monzo.rs` (module level, near `impl Monzo`):
```rust
/// Map a Monzo response status to the right error, or `None` when successful.
/// `auth_sensitive`: 401/403 mean the token was rejected → [`BankError::AuthExpired`]
/// (the 90-day re-auth path); endpoints where 401/403 is "merely" an error pass false.
fn status_error(status: reqwest::StatusCode, what: &str, auth_sensitive: bool) -> Option<BankError> {
    if auth_sensitive
        && (status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN)
    {
        return Some(BankError::AuthExpired(format!(
            "Monzo {what} rejected ({status})"
        )));
    }
    if !status.is_success() {
        return Some(BankError::Other(anyhow::anyhow!("Monzo {what} error {status}")));
    }
    None
}
```

- [ ] **Step 2: Replace the four sites**

Transactions (~224-234) — replaces BOTH the 401/403 arm and the generic arm:
```rust
            if let Some(e) = status_error(resp.status(), "transactions", true) {
                return Err(e);
            }
```
Accounts (~298-303):
```rust
        if let Some(e) = status_error(resp.status(), "accounts", false) {
            return Err(e);
        }
```
Pots (~330-333):
```rust
        if let Some(e) = status_error(resp.status(), "pots", false) {
            return Err(e);
        }
```
Token refresh (~381-392):
```rust
        if let Some(e) = status_error(resp.status(), "token refresh", true) {
            return Err(e);
        }
```
Error *messages* change slightly (e.g. "Monzo access token rejected" → "Monzo transactions rejected (401 Unauthorized)"); existing tests assert `matches!(err, BankError::AuthExpired(_))`, not exact strings — verify with the gate, and if any test does pin a message, update the assertion to the variant check.

- [ ] **Step 3: Run the full gate, then commit**

```bash
git add src/banks/monzo.rs
git commit -m "refactor(monzo): single status_error helper for the four HTTP status blocks"
```

---

### Task 14: Feature-gate `tiny_http` + `webbrowser` (`auth-flow`)

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/run.rs` (`auth_monzo` + its private helpers `random_state`, `verify_state`, `urlencoding` and their tests)
- Modify: `src/main.rs` (Auth arm)
- Modify: `Dockerfile:11`
- Modify: `.github/workflows/release.yml` (test job)
- Modify: `docs/RUNBOOK.md` (note)

- [ ] **Step 1: Make the deps optional**

In `Cargo.toml`:
```toml
[features]
default = ["auth-flow"]
# Interactive Monzo OAuth bootstrap (`aba auth monzo`) — dev machines only.
# The production container is built --no-default-features: no local HTTP
# listener / browser-opener code in the prod binary.
auth-flow = ["dep:tiny_http", "dep:webbrowser"]
```
and change the two dependency lines:
```toml
tiny_http = { version = "0.12", optional = true }
webbrowser = { version = "1", optional = true }
```

- [ ] **Step 2: Gate the code**

In `src/run.rs`, annotate the auth-only items:
```rust
#[cfg(feature = "auth-flow")]
fn random_state() -> anyhow::Result<String> { ... }

#[cfg(feature = "auth-flow")]
fn verify_state(expected: &str, returned: Option<&str>) -> anyhow::Result<()> { ... }

#[cfg(feature = "auth-flow")]
pub async fn auth_monzo(cfg: &Config) -> anyhow::Result<()> { ... }

#[cfg(feature = "auth-flow")]
fn urlencoding(s: &str) -> String { ... }
```
First confirm `urlencoding`/`random_state`/`verify_state` have no callers outside `auth_monzo` (`rg 'urlencoding|random_state|verify_state' src/`); if any test functions cover them, gate those tests with `#[cfg(feature = "auth-flow")]` too.

In `src/main.rs`, the Auth arm becomes:
```rust
        Command::Auth {
            which: AuthCommand::Monzo,
        } => {
            #[cfg(feature = "auth-flow")]
            {
                run::auth_monzo(cfg).await
            }
            #[cfg(not(feature = "auth-flow"))]
            {
                anyhow::bail!(
                    "this binary was built without the auth-flow feature (production image); \
                     run `aba auth monzo` from a dev build instead"
                )
            }
        }
```

- [ ] **Step 3: Build the prod image without it**

`Dockerfile:11`:
```dockerfile
RUN cargo build --release --bin aba --no-default-features
```
And in `.github/workflows/release.yml` test job, after the clippy line add both-config checks:
```yaml
      - run: cargo check --no-default-features
      - run: cargo clippy --all-targets --no-default-features -- -D warnings
```

- [ ] **Step 4: RUNBOOK note**

Add one line to `docs/RUNBOOK.md` in the auth/bootstrap section: the container image is built `--no-default-features` and will refuse `aba auth monzo` with a pointer to run it from a dev build (which it always was — it needs a browser).

- [ ] **Step 5: Verify both configurations + gate, then commit**

```bash
cargo check --no-default-features
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test          # default features: full suite
```
```bash
git add Cargo.toml Cargo.lock src/run.rs src/main.rs Dockerfile .github/workflows/release.yml docs/RUNBOOK.md
git commit -m "feat: gate dev-only OAuth bootstrap behind auth-flow feature; prod image excludes it"
```

---

## Final acceptance (after Task 14)

- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean (default features AND `--no-default-features`)
- [ ] `cargo test` — 151 pre-existing tests + the new ones, all green
- [ ] `git log --oneline` shows one commit per task (14 commits)
- [ ] Phase B plan authoring is the next action (run.rs decomposition), written against this new baseline
