# Monzo Transaction Pagination — Design

**Date:** 2026-06-02
**Status:** Approved for planning
**Phase 2 backlog item:** #3 (see `docs/PHASE2-BACKLOG.md`)

## Overview

`Monzo::fetch_since` currently issues a **single** `GET /transactions` request
with `limit=100` and only emits a `warn!` when exactly 100 rows come back — it
does **not** paginate. High-volume windows are therefore silently truncated. The
most exposed case is the **first ever run**, which fetches a 30-day window
(`run.rs`: `since = state.last_run.unwrap_or(now - 30 days)`); an active spender
can exceed 100 transactions in 30 days.

This change makes `fetch_since` walk **all** pages for the requested window using
Monzo's cursor pagination, with a generous runaway guard that fails loudly rather
than ever silently dropping transactions.

## Goals

- Fetch every transaction in the `[since, now]` window, regardless of count.
- Use Monzo's **cursor** pagination (last transaction `id` as the next `since`),
  which is immune to the same-`created`-timestamp skip/duplicate hazard that pure
  time-based paging suffers at page boundaries.
- Never silently truncate: a runaway guard aborts the run loudly so no data is
  lost (see "Why fail loudly").
- Preserve all existing `fetch_since` behaviour: decline filtering, merchant /
  description fallback, error mapping (`AuthExpired` on 401/403, `Other`
  otherwise), and the `RawTransaction` shape.

## Non-goals (this feature)

- Changing `run.rs`, `last_run` semantics, dedup, or the import path.
- Configurable page size or cap (a compile-time constant is sufficient — YAGNI).
- Parallel / concurrent page fetches (the loop is strictly sequential because
  each page's cursor depends on the previous page's last row).
- GoCardless, hardening, or other Phase 2 items.

## Monzo pagination semantics (confirmed via Context7 / docs.monzo.com)

`GET /transactions` supports both time-based and cursor-based pagination:

- `since` — **either** an RFC3339 timestamp **or** an object `id`. As an `id` it
  is a cursor: only objects created *after* that id are returned.
- `before` — RFC3339 timestamp upper bound (not used here).
- `limit` — page size; default and **maximum 100**.

Transactions are returned **oldest-first**. Forward iteration is therefore:

```
page 1: since = <window-start RFC3339 timestamp>, limit = 100
page 2: since = <id of last row from page 1>,     limit = 100
page N: since = <id of last row from page N-1>,   limit = 100
stop when a page returns < 100 raw rows (the final, partial page)
```

## Design

### Loop structure

`fetch_since` keeps its current preamble unchanged (access-token presence check,
empty-`account_id` check). It then replaces the single request with a loop:

1. Maintain a `cursor: String` initialised to `since.to_rfc3339()`.
2. Each iteration issues the existing request, substituting `("since", &cursor)`.
   The `account_id`, `expand[]=merchant`, and `limit=100` query params are
   unchanged. Error handling (401/403 → `AuthExpired`; non-success → `Other`;
   transport / deserialize → `Other`) is unchanged and applies per page.
3. Deserialize into `TxList`. Let `raw_len = list.transactions.len()`.
4. Append `list.transactions` (raw `MonzoTx`) into an accumulator `Vec<MonzoTx>`.
5. **Page-size check is on the raw count**, and the **cursor is the last raw
   `id`** — both *before* decline filtering, so filtered-out declined rows can
   never shorten a page and falsely signal "last page".
   - If `raw_len < 100` → final page; break.
   - Else set `cursor = <last raw row's id>` and continue.
6. Runaway guard: a `MAX_PAGES = 100` constant (≈10,000 transactions). If the
   loop would exceed it, return `BankError::Other` (see below). This is a
   bug/abuse backstop, not an expected path.

After the loop, the **accumulated raw rows** are filtered (drop `decline_reason`)
and mapped to `RawTransaction` exactly as today — the existing map closure moves
verbatim from per-response to the accumulator.

### Why fail loudly (cap behaviour)

If the cap were "warn and proceed with what we have", we would keep the
**oldest** pages and drop the **newest** ones (oldest-first ordering). `run.rs`
then advances `last_run` to `now()` on success, so the dropped middle window is
**lost forever**. Aborting with `BankError::Other` instead means `fetch_since`
returns `Err`, the `?` in `run.rs` propagates it, `last_run` is **not** advanced,
and the next run simply re-fetches the same window. Because imports dedup on
`imported_id = external_id`, the retry is harmless. Loud failure is strictly
safer than silent partial success, and at 10,000 transactions the cap should
never trigger for a personal account.

### Edge cases

- **Empty window**: first page returns `< 100` (often 0) → single request, break,
  empty result. Same as today.
- **Exactly 100, then 0**: page 1 returns 100 (so we page again); page 2 returns
  0 (`< 100`) → break. Correctly terminates with the 100 rows. (The old code
  would have warned and stopped at 100, possibly missing nothing or missing a
  lot — now deterministic.)
- **All rows on the last page declined**: `raw_len` still reflects the raw count,
  so termination is unaffected; the output simply contains fewer mapped rows.

## Error handling

Unchanged per-page semantics:

- `401`/`403` on any page → `BankError::AuthExpired`.
- Any other non-success status, transport error, or body deserialize failure on
  any page → `BankError::Other`.
- Exceeding `MAX_PAGES` → `BankError::Other` with a descriptive message
  (mentions the cap and that the run is aborted to avoid silent truncation).

## Testing

`wiremock` is already the test harness for this module. Plan to add:

1. **Two-page walk**: mock returns 100 rows on the first request (cursor =
   timestamp) and a short final page on the second (cursor = last id), asserting
   the combined result length and that page-2 was requested with
   `since=<last-id-of-page-1>`. (Use `wiremock` request matching on the `since`
   query param, or sequenced responses, to prove the cursor advanced.)
2. **Single-page (< 100)**: existing-style response returns immediately; result
   matches today's behaviour (regression guard for the common path).
3. **Decline on the boundary**: a full page of 100 where some are declined still
   triggers a second fetch (proves the page-size check uses the raw count, not
   the filtered count).
4. **Runaway cap**: a mock that always returns 100 rows causes `fetch_since` to
   return `BankError::Other` after `MAX_PAGES` requests (not an infinite loop).
   To keep the test fast, this may warrant making `MAX_PAGES` overridable in
   `#[cfg(test)]` *or* asserting the error without forcing all 100 round-trips —
   resolved in the implementation plan.

Existing tests (`fetch_maps_merchant_with_description_fallback_and_skips_declined`,
`fetch_with_empty_account_id_errors`, `fetch_403_is_auth_expired`) must continue
to pass unchanged; the empty-account-id and 403 paths are exercised on the first
page.

## Harness notes (from project memory)

- Verify with `cargo build` / `cargo clippy --all-targets -- -D warnings` /
  `cargo test` and read the **actual** output; treat `<new-diagnostics>` as
  advisory only.
- `aba` is a **binary** crate — run `cargo test` (optionally `cargo test monzo`),
  never `cargo test --lib`.
