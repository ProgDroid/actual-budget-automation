# aba Runbook

## One-time Monzo bootstrap (on a machine with a browser)
1. Register a Monzo OAuth client at https://developers.monzo.com with redirect
   URI `http://localhost:8080/callback` (confidential client).
2. Export env: `MONZO_CLIENT_ID`, `MONZO_CLIENT_SECRET`,
   `MONZO_REDIRECT_URI=http://localhost:8080/callback`, plus the other vars below.
3. Run `aba auth monzo`. Approve in the Monzo app when prompted (SCA).
4. Copy the printed `refresh_token` into the server state file under `[monzo]`,
   and set `[monzo].account_id`.

## Daily run (server, one-shot container)
`docker run --rm --network <actual-net> \
  -e ANTHROPIC_API_KEY -e MONZO_CLIENT_ID -e MONZO_CLIENT_SECRET \
  -e MONZO_REDIRECT_URI -e APPRISE_URL \
  -e ACTUAL_SERVER_URL=http://actual:5006 -e ACTUAL_PASSWORD -e ACTUAL_SYNC_ID \
  -e ABA_STATE_PATH=/data/aba.state.toml \
  -v /srv/aba:/data \
  ghcr.io/progdroid/actual-budget-automation:latest run`

Schedule with cron, e.g. `0 3 * * *`.

## Dry run (safe; no writes to Actual)
Append `--dry-run` to the `run` command above. Reviews are not recorded and
`last_run` is not advanced; only a rotated Monzo token is persisted.

## Resolving held payees
Edit the state file's `[[pending_review]]` entries: set `resolve_to` to an
existing payee, or `create_new = true`. The next run imports them and caches the
mapping.

## Required environment variables
ANTHROPIC_API_KEY, CLAUDE_MODEL (optional, default claude-sonnet-4-6),
MONZO_CLIENT_ID, MONZO_CLIENT_SECRET, MONZO_REDIRECT_URI, APPRISE_URL,
ACTUAL_SERVER_URL, ACTUAL_PASSWORD, ACTUAL_SYNC_ID, ABA_STATE_PATH,
ABA_SHIM_DIR (optional, default /app/actual-shim).
