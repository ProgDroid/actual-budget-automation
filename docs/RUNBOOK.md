# aba Runbook

## One-time Monzo bootstrap (on a machine with a browser)
1. Register a Monzo OAuth client at https://developers.monzo.com with redirect
   URI `http://localhost:8080/callback` (confidential client).
2. Export env: `MONZO_CLIENT_ID`, `MONZO_CLIENT_SECRET`,
   `MONZO_REDIRECT_URI=http://localhost:8080/callback`, plus the other vars below.
3. Run `aba auth monzo`. Approve in the Monzo app when prompted (SCA).
4. Copy the printed `refresh_token` into the server state file under `[monzo]`,
   and set `[monzo].account_id`.

## Account mapping (multi-account)

Transactions are routed to Actual accounts via `[[monzo.accounts]]` links in the
state file (`monzo_id` → `actual_id`). Bootstrap them once:

1. Ensure Monzo is authed (`aba auth monzo`) and a `refresh_token` is in state.
2. Run `aba accounts` to preview the auto-suggested mapping (Monzo account names
   and pot names each matched to Actual account names by similarity).
3. Run `aba accounts --write` to persist it.
4. Review the state file's `[[monzo.accounts]]` entries; fix any `actual_id` that
   is empty (UNMAPPED) or wrong by editing it directly.

**Pot links (`[[monzo.pots]]`):** `aba accounts` also suggests a Monzo pot →
Actual account mapping and `--write` persists it alongside the account links. For
each pot you want to track as a transfer target, create a matching Actual account
(if one doesn't already exist; on- or off-budget — aba only needs its id), then
confirm or fix the mapping by
editing `[[monzo.pots]]` in the state file. Pot-move transactions whose pot is
unmapped are HELD and retried every run (surfaced in the run summary as
"transfer(s) awaiting account mapping"); they import automatically once you map the
pot. Pots you leave unmapped are simply held — no data is lost.

**Migration:** an old state file with a single `[monzo] account_id` is auto-promoted
on first load to one UNMAPPED `[[monzo.accounts]]` link (the old value is the Monzo
id; the Actual side must be supplied via `aba accounts`). Until mapped, transactions
from that account are HELD (never imported to a wrong/blank account) and surfaced in
the run summary.

## Daily run (server, one-shot container)

The container runs as the non-root `node` user (uid 1000, set by `USER node` in
the `Dockerfile`). The host bind-mount directory (`/srv/aba` → `/data`) holds
both the state file and the Actual cache (`ACTUAL_DATA_DIR=/data/actual-cache`),
so it must be writable by uid 1000 or the run cannot persist state. One-time
setup on the host:

```sh
sudo mkdir -p /srv/aba
sudo chown -R 1000:1000 /srv/aba
```

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

> **Note (categories):** a held entry now imports only once **both** its payee
> **and** its category are settled (see *Categories* below). When resolving by
> hand, also set the category dimension if it is open: `resolve_category_to`
> to an existing category name (empty = leave uncategorised) **and**
> `category_resolved = true`. An entry whose category is still unsettled
> (`category_resolved = false`) stays held even after the payee is resolved.

## Categories (Phase 2)

`aba run` asks Claude to classify each new merchant into one of your **existing
Actual categories**, in the same call that picks the payee. A transaction is
imported automatically only when **both** the payee and the category are
confident matches to things that exist in Actual. If either is uncertain, the
whole transaction is **held** and you are asked via Telegram — first for the
payee (if that is the uncertain part), then for the category.

- **Confirm** accepts the suggested category. **Skip** leaves the transaction
  uncategorised (it still imports). Reply with a different category name to remap.
- Categories are cached per merchant (like payees), so each merchant is normally
  only asked once.
- **No "create new category" via Telegram.** Actual categories must belong to a
  category group, so create new categories in the Actual UI, then reply with the
  new category's name (or let the next run re-suggest it).
- If you have **no categories** in Actual at all, categorisation is skipped
  entirely and transactions import uncategorised (Phase-1 behavior).

## Operational notes

- **Prompt caching gives no benefit at the nightly cadence.** Anthropic prompt
  caching uses an ~5-minute ephemeral TTL that expires between once-daily runs,
  so it only pays off for runs <5 minutes apart (retries / local testing) or a
  higher-frequency schedule. Revisit if the schedule changes.

## Required environment variables
ANTHROPIC_API_KEY, CLAUDE_MODEL (optional, default claude-sonnet-4-6),
MONZO_CLIENT_ID, MONZO_CLIENT_SECRET, MONZO_REDIRECT_URI, APPRISE_URL,
ACTUAL_SERVER_URL, ACTUAL_PASSWORD, ACTUAL_SYNC_ID, ABA_STATE_PATH,
ABA_SHIM_DIR (optional, default /app/actual-shim).
