---
name: fetch-failloud-invariant
description: "aba architectural invariant — bank fetches must fail loudly (Err), never return a partial Ok, or last_run advances and data is lost forever"
metadata: 
  node_type: memory
  type: project
  originSessionId: b512701a-764f-41cd-aa84-307a3742e19d
---

In aba, `run.rs` advances `state.last_run = Some(Utc::now())` **only after a successful run** (and only when `!dry_run`); a `fetch_since` error propagates via `?` *before* that line, so `last_run` is NOT advanced and the next run re-fetches the same window. The next run's window is `since = last_run.unwrap_or(now - 30 days)`.

**Why:** This makes a hard rule for every `BankSource::fetch_since` implementation: it must **fail loudly (return `Err`) rather than ever return a partial `Ok`**. If a fetch silently returns an incomplete set (e.g. truncated pagination, a swallowed page error), `last_run` jumps to `now()` and the dropped transactions are **lost permanently** — there is no reconciliation pass. This is exactly why Monzo pagination (#3) aborts with `BankError::Other` on the runaway cap and on an un-advanceable cursor instead of breaking with what it has. Re-fetching overlap on a retry is harmless because imports dedup on `imported_id = external_id`.

**How to apply:** When building #2's replacement aggregator source (GoCardless is blocked — see [[gocardless-blocked]]) or any new write path, never trade completeness for "at least we got something". Partial-but-silent is the worst outcome; a loud failure that retries next run is correct. See [[phase2-progress]] and [[server-constraints]].
