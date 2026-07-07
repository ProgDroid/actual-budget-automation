# Category Picker for Telegram Prompts — Design

**Date:** 2026-07-07
**Status:** Approved (brainstorming), pending implementation plan

## Problem

When a transaction's payee is settled but its category is still open, the bot
asks the user to **type a category name** as a free-text Telegram reply. Payee
typing is fine, but category names are sometimes long/complicated and hard to
recall exactly, and a typo produces a category that fails loud at import. The
user wants a **button picker** for categories instead of typing.

Scope: **categories only.** The payee prompt flavors are unchanged.

## Decisions (from brainstorming)

- **Scale:** ~15–30 categories → a single flat button grid in one message is fine.
- **Layout:** flat list, **2 buttons per row**, no category-group headers.
  (Column count is trivially tunable later.)
- **Suggestion handling:** when Claude suggests a category, show a one-tap
  `✅ Confirm: <suggestion>` shortcut **first**, then the full picker grid, then
  Skip. When there is no suggestion: grid + Skip only.
- **Free-text fallback retained:** typing a category name as a reply still works
  exactly as today (zero cost, safety net).
- **Deployment unchanged:** stays a one-shot cron container (see "Cron" below).

## UX

**Category prompt, with a suggestion** (`SQ *CAFE` → `Dining`, medium confidence):

```
Category for 'SQ *CAFE' → 'Dining'?  (medium confidence)
Tap a category below, or reply with a name.
[ ✅ Confirm: Dining                     ]   ← existing "cc" action
[ Groceries      ][ Dining               ]   ← full picker, 2/row, all live categories
[ Transport      ][ Bills                ]
[ … (15–30 total)                        ]
[ ❌ Skip (leave uncategorised)          ]   ← existing "xc" action
```

**Category prompt, no suggestion:** identical grid + Skip, without the top
Confirm row. Text: `Which category for 'SQ *CAFE'? Tap below, or reply with a name.`

The suggested category also appears in the grid (the grid is the full list); the
top row is just a labeled shortcut. No special-casing to exclude it.

## Encoding — how a tapped button resolves to a category

Each category button carries:

```
callback_data = "pc:<group_key>:<cat_hash>"
```

- `group_key` — existing 16-hex FNV-1a hash of `raw_merchant` (unchanged).
- `cat_hash` — **16-hex FNV-1a hash of the category name**, produced by the same
  hashing primitive as `group_key`.
- Total ≈ 36 bytes, well under Telegram's 64-byte `callback_data` cap.

**Resolution at reply time:** the bot re-hashes the live category list
(`state.actual_category_names`, already persisted across runs) and finds the name
whose hash equals `cat_hash`, then applies it via the existing
`State::remap_category`. This mirrors the established `group_key` correlate-by-
rehash pattern.

**Why hash the name, not a positional index:** a hash resolves correctly even if
categories are added/reordered in Actual between the prompt being sent and the
tap. A positional index would silently point at the wrong category. If the
category was **deleted** in the interim, no hash matches → the entry stays held
and re-prompts with a fresh grid next run (fail-safe, never fail-silent).

## Structural change: `Prompt` carries rows

Today `Prompt.buttons: Vec<(String, String)>` renders as a **single** inline-
keyboard row (`build_send_request` emits `inline_keyboard: [row]`). A multi-row
picker needs real rows, so:

- Change `Prompt.buttons` to **`Vec<Vec<(String, String)>>`** (rows of
  `(label, callback_data)`).
- `build_send_request` emits `inline_keyboard: <rows>` directly (no chunking).
- Existing payee prompts become one row each (visual result unchanged).
- The picker builds: `[Confirm row?]` + `[grid rows, 2/row]` + `[Skip row]`.

This is the correct model rather than hacking column-chunking into the transport.
It rewrites a handful of existing prompt-builder test assertions (`buttons.len()`,
`buttons[0].1`) to index into rows.

## Reply plumbing

- New `ReplyAction::PickCategory(String)` carrying `cat_hash`.
- New action code `"pc"`.
- `parse_get_updates`: `parse_callback` splits on the first `:` → `("pc",
  "<group_key>:<cat_hash>")`. For code `pc`, split the remainder again into
  `(group_key, cat_hash)`; emit `Reply { entry_key: group_key, action:
  PickCategory(cat_hash) }`. All other codes are unchanged.
- `poll.rs`: **before** the per-member mutation loop (where `state` is borrowed
  mutably), resolve `cat_hash` → `Option<category name>` by scanning
  `state.actual_category_names` (hoisted, like the existing `payee_settled`
  computation). In the loop, `PickCategory` → `state.remap_category(ext, &name)`
  when a name resolved; when none resolved, no-op (entry stays held → re-prompt).
- `answerCallbackQuery` acking is unchanged (every callback id is acked to clear
  the spinner). PickCategory settles the **category** dimension, so it does not
  trigger the "payee settled → re-prompt for category" clearing block. Parity
  with the existing `cc`/`xc` behavior (answered keyboard is left in chat).

## Cron vs long-lived (decided: stays cron)

The picker requires **no** change to the deployment model. A tapped button is a
`callback_query` that Telegram queues server-side (~24h); the bot drains it on the
next cron run via `getUpdates` using the persisted `last_update_id` offset —
identical to how `✅ Confirm`/`❌ Skip` taps are handled today. The only
trade-off (unchanged from today): a tap is applied on the next cron tick, not
instantly, and the button spinner clears on the next poll rather than
immediately. Making the bot long-lived would buy only faster spinner-clearing /
lower latency, at the cost of a persistent process, restart handling, and single-
instance locking to avoid double-draining `getUpdates`. Not worth it at personal-
finance cadence.

## Known limitation (pre-existing, not worsened)

Resolution is by category **name**. If two Actual category groups contain an
identically-named category (e.g. two "Other"s), the picker resolves to whichever
the live list yields first — the same ambiguity as typing "Other" today. Fixing
it properly would require category **ids** in the callback, which don't fit the
byte budget. Out of scope.

## Test impact

- Update prompt-builder tests for the rows model (`Vec<Vec<_>>` indexing).
- New tests: `pc` callback round-trip (encode → `parse_get_updates` →
  `PickCategory`); picker grid built for suggestion and no-suggestion flavors;
  `cat_hash` resolution to a live name; deleted-category (no match) stays held;
  `build_send_request` emits multiple rows.
- Existing free-text category remap, confirm-category, skip-category tests remain
  valid.

## Out of scope

- Payee prompt changes.
- Category-group headers / two-step (group → category) picker.
- Category-id-based resolution / duplicate-name disambiguation.
- Long-lived bot / webhooks.
