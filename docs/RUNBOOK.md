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

## Telegram confirm loop

Uncertain Claude payee matches (medium/low confidence) and no-match transactions
are held as `pending_review` entries and sent to a Telegram bot for you to
confirm or correct from your phone. Use two cron jobs instead of one:

```cron
*/15 * * * *  docker run --rm --network <net> -v /srv/aba:/data <image> poll
0   3 * * *   docker run --rm --network <net> -v /srv/aba:/data <image> run
```

`poll` (~15 min) drains the Telegram mailbox and applies replies to the state
file; it never touches Actual. `run` (nightly) imports everything resolved during
the day and sends prompts for any newly held items.

### New environment variables

| Variable | Default | Description |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | — | Bot API token (from BotFather). Optional; if unset, Telegram is disabled. |
| `TELEGRAM_CHAT_ID` | — | Your numeric chat id. Only this chat may drive resolutions. |
| `ABA_AUTOIMPORT_CONFIDENCE` | `high` | Auto-import cutoff: `high` (default), `medium`, or `low`. |
| `ABA_LOCK_PATH` | `<state>.lock` | Path to the advisory lock file (beside the state file by default). |
| `TELEGRAM_BASE_URL` | `https://api.telegram.org` | Override for self-hosted Bot API servers. |

### Bot setup (one-time)

1. Message `@BotFather` on Telegram: `/newbot`, follow the prompts, copy the
   token into `TELEGRAM_BOT_TOKEN`.
2. Send any message to your new bot (it must receive at least one message before
   `getUpdates` returns your chat id).
3. Fetch your numeric chat id once:
   ```
   https://api.telegram.org/bot<token>/getUpdates
   ```
   Copy the `"id"` value from `result[0].message.chat` into `TELEGRAM_CHAT_ID`.
4. Export both env vars in your cron / docker-compose environment.

### Fire exit (Telegram down or unconfigured)

If Telegram is unconfigured or unavailable, held items accumulate in the state
file as usual. Edit `[[pending_review]]` entries directly:
- Set `resolve_to` to an existing payee name, **or**
- Set `create_new = true`.

The next `aba run` imports them. Nothing breaks if Telegram is unreachable.

## Required environment variables
ANTHROPIC_API_KEY, CLAUDE_MODEL (optional, default claude-sonnet-4-6),
MONZO_CLIENT_ID, MONZO_CLIENT_SECRET, MONZO_REDIRECT_URI, APPRISE_URL,
ACTUAL_SERVER_URL, ACTUAL_PASSWORD, ACTUAL_SYNC_ID, ABA_STATE_PATH,
ABA_SHIM_DIR (optional, default /app/actual-shim).
