---
name: monzo-token-rotation-invariant
description: aba invariant — Monzo refresh tokens are single-use/rotating; persist the rotated token to disk BEFORE any fallible work or the next run gets 401-locked out
metadata: 
  node_type: memory
  type: project
  originSessionId: 94da65b7-91ab-40e1-aed7-bd649ba7cfea
---

Monzo rotates the refresh token on **every** successful `/oauth2/token` refresh and **invalidates the previous one server-side** (single-use). So the instant `Monzo::ensure_auth` succeeds, the on-disk token is already dead; only the in-memory `state.monzo.refresh_token` holds the valid replacement.

**Why:** Before the fix, `run()` only persisted state at the very end (after `fetch_since` + Actual reads/writes). Any failure in between — e.g. the Actual TLS/connectivity issue in [[actual-connection-tls]] — returned early via `?`, stranding the rotated token in memory. The on-disk (now-invalidated) token was then replayed on the next run → Monzo `401` → `AuthExpired("Monzo refresh rejected (401 Unauthorized)")`, locking the user out until a manual `aba auth monzo` re-auth on a browser machine. This is the same data-integrity family as [[fetch-failloud-invariant]]: an irreversible remote side effect must be made durable locally **before** any fallible step, never after. Note the trap ordering in `run()`: `ensure_auth` (rotates) → `fetch_since` → `get_payees` (was failing) → end-of-run save.

**How to apply:** Fixed on branch `fix/persist-rotated-monzo-token` — `persist_rotated_token()` writes ONLY the rotated token (preserving cache/pending/`last_run`) immediately after a successful `ensure_auth`, guarded by regression test `rotated_token_persisted_when_later_step_fails`. NEVER move token persistence back to end-of-run, and keep this property when refactoring `run()`. Recovery when a token IS burned: run `aba auth monzo` on the Windows dev box and paste the new `refresh_token` into the `[monzo]` section of the state file. See [[phase2-progress]].
