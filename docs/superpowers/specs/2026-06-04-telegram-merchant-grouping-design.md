# Telegram Prompts Grouped Per Merchant — Design (Phase 4)

**Date:** 2026-06-04
**Status:** Approved (brainstorming) — pending written-spec review

## Background

This is **Phase 4 of four** in the multi-account aba enhancement
(`docs/superpowers/specs/2026-06-03-multi-account-routing-design.md`). Phases 1–3
(account routing, categories, transfers/Flex) are done. The Telegram
confirm/correct loop itself already exists — it was Phase-2 backlog item #1
(`docs/superpowers/specs/2026-06-01-telegram-confirm-loop-design.md`). Phase 4 is
a **refinement** of that loop, not a new feature.

## Problem

The unit of "held for review" is the **transaction** (`PendingReview`, keyed by
`external_id`), but the unit of **resolution** is the **merchant**
(`payee_cache` and the category cache both key on the exact `raw_merchant`
string, and Claude is asked once per unique `raw_merchant`).

Because the send loop (`run.rs:1240`) emits one Telegram message per held
`PendingReview`, a merchant that recurs in a single run — e.g. three "TFL TRAVEL
CH" trips — produces **three near-identical prompts**, all of which resolve to
the same answer. The user wants **one prompt per merchant**, whose single reply
resolves **every** held transaction of that merchant.

## Scope

In scope (Phase 4):

- Collapse all held transactions sharing an **exact `raw_merchant`** into a
  single Telegram prompt.
- A single reply (button or free text) fans out to **every** held transaction of
  that merchant.
- New transactions of a merchant that already has an outstanding prompt **join**
  it silently (no second prompt).
- The run summary counts **merchants** awaiting confirmation, not transactions.

Out of scope:

- Normalised/fuzzy merchant grouping — **explicitly rejected** (see Decisions).
- Cross-merchant digest messages — explicitly rejected (collapse same-merchant
  only).
- Any change to the `poll`/`run` split, confidence gating, or the import path.
- Any `PendingReview` schema change (grouping is derived, not stored).

## Approach (chosen: "A" — derived grouping)

A merchant group is a **derived view**, never stored: bucket
`state.pending_reviews` by exact `raw_merchant` wherever prompts are sent
(`run`) or replies are applied (`poll`). `PendingReview` stays per-transaction
(each needs its own `external_id`/`amount`/`date` to import idempotently).

Rejected alternatives:

- **B — first-class group in state** (`group_id` field or a `[[merchant_group]]`
  table): adds a schema change, a migration, and a second source of truth that
  can desync from the entry list. YAGNI for a single-user tool.
- **C — send-only collapse** (one message, per-transaction replies): impossible —
  a single message carries one inline keyboard and one `reply_to` id, so replies
  are inherently group-level.

## Grouping key

The Telegram `callback_data` cap is 64 bytes and `raw_merchant` is unbounded and
may contain `:` (the callback delimiter), so the raw string cannot be the key.

New `telegram::group_key(raw_merchant: &str) -> String`: a deterministic
**FNV-1a 64-bit** digest rendered as 16 lowercase hex chars. Implemented inline
(no dependency; FNV-1a is ~5 lines and version-stable by construction, unlike
`std::hash::DefaultHasher` whose output is not contractually stable). 16 hex
chars + the longest `<action>:` prefix (`nw:`, `cf:`, `sk:`, `cc:`, `xc:`) stays
far under 64 bytes, so the keyboard is **never** dropped for length (today's
over-long-`external_id` fallback in `build_prompt` becomes effectively dead for
grouped prompts, but is retained as defence in depth).

`poll` recomputes `group_key` from the live pending list, so the key need not be
stored. Collision risk (two distinct merchants → same 64-bit digest) is
astronomically small and **accepted**; on the vanishing chance it occurred, both
merchants would resolve together — documented, not guarded.

## Component changes

### `src/telegram.rs`

- Add `pub fn group_key(raw_merchant: &str) -> String` (FNV-1a, 16 hex).
- `Reply.entry_key` semantics change from "an `external_id`" to "a **group
  key**". No struct change; `poll_replies` already derives `entry_key` from the
  button `callback_data` or the `msg_map` — both now carry group keys.
- `poll_replies`'s `msg_map` parameter type is unchanged (`BTreeMap<i64,
  String>`) but its **value** is now a group key, not an `external_id`.

### `src/run.rs` — `build_prompt`

- Signature gains the member count and uses the group key. Sketch:
  `build_prompt(rep: &PendingReview, member_count: usize, payee_names, category_names)`.
- Callback key = `group_key(&rep.raw_merchant)` instead of `&rep.external_id`.
- Text gains the count: `'TFL TRAVEL CH' (3 txns) -> 'Transport for London'?
  (medium confidence)`. Singular form for a single member (`(1 txn)` or omit the
  suffix — decided in the plan; prefer always showing `(N txns)` for
  consistency). **Merchant + count only** (no amounts/total — user decision).
- All payee flavors (confirm/create/skip, create/skip, which-payee) and both
  category flavors (confirm/skip, which-category) are unchanged except for the
  key and the count.

### `src/run.rs` — send loop (currently `run.rs:1240–1260`)

Replace the per-entry loop with a per-group loop:

1. Bucket `pending_reviews` indices by `raw_merchant` (preserve a stable order —
   e.g. first-seen — for deterministic tests).
2. For each bucket, compute the **current dimension**:
   - payee, if **any** member's payee is unsettled (`!payee_settled()`);
   - else category, if **any** member's category is unsettled
     (`!category_resolved`);
   - else none → the group's transactions are fully resolved and will import;
     send nothing.
3. **Idempotency (adopt-don't-resend):** if any member already carries a live
   `telegram_message_id`, that is the group's outstanding prompt for the current
   dimension (the payee→category transition clears ids in `poll`, so a live id
   always matches the current dimension). Copy that id onto any member lacking
   one (a new transaction that joined the merchant) and **do not** send.
4. Otherwise build one prompt from a **representative member at the current
   dimension** (members share `raw_merchant`, hence identical
   suggestion/confidence for that dimension) plus the member count, send it, and
   set the returned `message_id` on **all** members of the bucket. Persist
   immediately (existing M6 behaviour) so a later failure can't strand the id.

### `src/poll.rs`

- Build `msg_map: message_id -> group_key` from pending reviews
  (`group_key(&p.raw_merchant)`); entries sharing a merchant share both
  `message_id` and group key, so the map has no lossy collapse.
- `valid` set becomes the set of **group keys** present in pending reviews.
- For each reply: resolve `reply.entry_key` (a group key) to **all** member
  `external_id`s (`pending_reviews` whose `group_key(raw_merchant) ==
  entry_key`), then apply the existing per-entry mutator (`confirm_pending`,
  `create_pending`, `skip_pending`, `confirm_category`, `skip_category`,
  `remap_pending`/`remap_category`) to **each** member. Reuses tested mutators;
  no new state API.
- The free-text-remap dimension check (payee-settled?) is evaluated per member
  but is identical across members of a homogeneous group; for a (rare) cross-run
  heterogeneous group it is correct per member.
- The "clear `telegram_message_id` so `run` re-prompts for category" transition
  is applied to **all** members of the group, so the next prompt is again a
  single grouped category prompt.
- `applied` counts **groups** applied (for the log line), not members.

### Run summary

The "N item(s) awaiting your confirmation" line (`run.rs:1276`) counts **distinct
merchants** with an outstanding prompt, not transactions. Wording →
`N merchant(s) awaiting your confirmation.`

### Stale-prompt edit path (`run.rs:777`)

When a free-text remap fails to match and the entry is re-held, the best-effort
`edit_message_text` + `telegram_message_id = None` now operates on **all** group
members: edit the group's single old message once, clear the id on every member,
and re-prompt as one group on the next run.

## Cross-run / heterogeneous groups

Normally a group is homogeneous (same run, same Claude answer / cache miss), so
all members share dimension state. A heterogeneous group (same merchant held
across runs at different dimension states) is rare — once a merchant's payee is
resolved and imports, the `payee_cache` short-circuits future holds. It is
handled safely: the group's current dimension is the **least-settled** dimension
across members, the prompt is built from a representative at that dimension, and
the fan-out applies the action to all members (settling an already-settled
dimension is an idempotent no-op in the mutators — verified in the plan).

## Back-compat

No state schema change; old state files load unchanged. The **callback key
format** changes from `external_id` to `group_key`. A Telegram prompt left
outstanding **across the upgrade** carries old-format `external_id` buttons that
no longer match any group key, so its **button taps become inert**; its
**free-text reply still works** (correlated via `msg_map`). Because the current
build is not yet redeployed, there are expected to be **no in-flight prompts at
cutover**. Documented as a one-time caveat; not engineered around.

## Dry-run

Unchanged: `run --dry-run` sends no Telegram messages and makes no Actual writes;
grouping affects only how prompts are assembled, not whether they are sent.

## Testing approach (TDD, trait doubles)

Using the existing `FakeChannel` (`run::tests`) and `poll` tests:

1. **`group_key`** — deterministic for equal inputs, differs for different
   inputs, and `<action>:<group_key>` fits the 64-byte cap.
2. **Send collapse** — three held transactions with identical `raw_merchant`
   produce exactly **one** `send_prompt`; all three rows get the **same**
   `telegram_message_id`; text contains `(3 txns)`.
3. **Distinct merchants** — two different `raw_merchant`s → two `send_prompt`s.
4. **Reply fan-out (button)** — a `Confirm` on a group's key sets `resolve_to`
   on **all** members.
5. **Reply fan-out (free text)** — a free-text remap correlated via `msg_map`
   group key fans out to all members.
6. **Adopt-don't-resend** — a new member added to a merchant that already has a
   live `message_id` triggers **no** new `send_prompt` and inherits the existing
   id.
7. **Payee→category transition** — a grouped payee confirm clears
   `telegram_message_id` on all members; the next `run` emits exactly **one**
   grouped category prompt.
8. **Summary** — the awaiting line counts merchants, not transactions.
9. **Heterogeneous group** — mixed-dimension members resolve without error
   (idempotent mutators).
10. Existing per-item `build_prompt`/`poll` tests updated for the group-key
    callback format.

## Decisions locked (brainstorming, 2026-06-04)

- **Collapse same-merchant**, one reply resolves all its held transactions (not a
  cross-merchant digest).
- **Exact `raw_merchant`** grouping (not normalised) — one reply only ever
  resolves the exact cache key the user saw.
- **Prompt shows merchant + transaction count only** (no amounts/total).
- **Approach A** (derived grouping, no schema change).
- **Adopt-don't-resend** idempotency for transactions joining an
  already-prompted merchant.

## Open items for the implementation plan

- Singular vs always-`(N txns)` wording for a single-member group.
- Confirm each state mutator is a safe no-op on an already-settled member (for
  the heterogeneous-group fan-out).
- Exact bucket ordering for deterministic test assertions.
