# Design: Eliminate stranded pending reviews

Date: 2026-07-24
Status: Approved (pre-implementation)

## Problem

Transactions from certain merchants never appear in Actual and never resolve — they
sit in `state.toml` `[[pending_review]]` indefinitely. Diagnosed from a live
`state.toml`: **every stuck entry has exactly one of its two dimensions (payee /
category) settled and the other open, while still carrying a `telegram_message_id`.**

Root cause is in `src/telegram/prompts.rs`, `send_group_prompts_inner`:

```rust
if let Some(id) = members.iter().find_map(|&i| state.pending_reviews[i].telegram_message_id) {
    for &i in &members { state.pending_reviews[i].telegram_message_id = Some(id); }
    continue; // adopt, don't resend
}
```

Once a merchant group has **any** member carrying a `message_id`, `run()` never
sends another prompt for that group. That is only safe if the outstanding prompt is
still unanswered **and** asks the dimension the member needs. Neither is guaranteed:

- A prompt covers **one** dimension. When the payee is answered, the category is
  still open, but the only thing that sent the category prompt was `poll`'s
  one-tick `chain_after_payee_settle` — which fires only on a payee-settling reply,
  only for members present that tick, and only for `Confirm | CreateNew | Remap`.
  Answer a **category** while the payee is open? Nothing re-prompts the payee.
- A **new** transaction joining a merchant that already has a prompt **adopts that
  id** and is assumed covered, even though the existing prompt was built for — and
  possibly already answered by — other members in a different state.

Evidence (heterogeneous groups sharing one id): `MR FERNANDO MACEDO FERREIRA`
(7 entries, all `message_id = 135`; first payee-settled to "Santander Checking",
six payee-open), `Google` (`141`, both payee-open), `Miruna Tecusanu` (`145`, all
payee-settled + category-open). These cannot be resolved by any single reply.

The pipeline is fail-loud in that nothing is *dropped*, but "held forever with no
live prompt" is functionally identical to lost from Actual's point of view.

## Non-goals

- **24/7 Telegram poller.** Orthogonal: the stranding is a state-machine
  correctness bug, not a scheduling one. Going always-on would not fix it. The
  re-prompt invariant below holds identically under cron or a long-poll daemon, so
  this design does not block it. Separate spec.
- **Surfacing `importTransactions` `result.errors` in the shim.** A real silent-drop
  worth fixing (the shim discards the `errors` array), but a separate follow-up per
  user request ("once we deal with this").

## Design

### 1. The invariant is per *group*, not per entry

The unit remains the merchant group: bucket by exact `raw_merchant`, reply fan-out
by `group_key` (unchanged). Each group has one **current question**, payee-first:

```
Q(group) = Payee     if any member has payee unsettled
         = Category  else if any member has payee settled but category unresolved
         = None       (fully resolved -> imported by take_resolved on the next run)
```

Member state helpers already exist: `PendingReview::payee_settled()` and the
`category_resolved` field.

A group **needs a prompt** iff `Q` is `Some` and no member currently holds a live
prompt whose dimension equals `Q`:

```
needs_prompt(group) = Q(group).is_some()
                   && !group.any(|m| m.telegram_message_id.is_some()
                                  && m.prompted_dimension == Q(group))
```

A mixed group is in its **payee phase** first; once the payee answer fans out and
settles all payee-open members, `Q` flips to Category and the enforcer sends the
category prompt — same-tick when driven by `poll` (it runs the enforcer right after
applying the reply), otherwise on the next `run`. Converges in two phases, **one
message per group per phase**.
Category resolution for a payee-settled member is deferred (by design) until the
group's payee-open members are settled — same merchant, payee is the prerequisite.

### 2. State change — `prompted_dimension`

Add to `PendingReview` in `src/state.rs`:

```rust
#[serde(default)]
pub prompted_dimension: Option<PromptDimension>,
```

`PromptDimension` is a small enum serialized as a lowercase string (`"payee"` /
`"category"`), e.g. via `#[serde(rename_all = "lowercase")]`. It is stored on every
member of a group alongside the shared `telegram_message_id` and moves with it.

Additive with `serde(default)` → **no `CURRENT_STATE_VERSION` bump**; existing
`state.toml` files load unchanged (absent field → `None`).

### 3. The shared enforcer — one function, two callers

Extract into `src/telegram/prompts.rs`:

```rust
async fn ensure_prompts(
    state: &mut State,
    channel: &dyn ConfirmChannel,
    payee_names: &[String],
    category_names: &[String],
    state_path: &Path,
    restrict: Option<&BTreeSet<String>>,
)
```

For each group (respecting `restrict` as today): compute `Q`; if `needs_prompt`,
best-effort `edit_message_text` on the superseded `message_id` (reuse the existing
"✅ Payee set — now categorising…" tidy text when moving payee→category), send the
prompt built from a representative whose open dimension == `Q` (reuse `build_prompt`;
its rep selection is already payee-first), then stamp `(new_id, Q)` on all members of
the group and persist via `state.save`.

- **`run()`** (`src/pipeline/mod.rs`) calls it after import, replacing
  `send_group_prompts`.
- **`poll()`** (`src/poll.rs`) calls it after applying replies, replacing
  `chain_after_payee_settle` + `send_chained_category_prompts`. `poll` uses the
  `state.actual_payee_names` / `actual_category_names` catalogs already snapshotted
  by the last `run`, so category prompts still fire **same-tick** (no latency
  regression).

This **deletes** the bespoke poll-side chaining. `poll` no longer manually clears
`telegram_message_id`; the payee→category hand-off happens naturally: after a payee
reply, `open_dim` becomes Category while `prompted_dimension` still reads `Payee` →
mismatch → the enforcer re-prompts on the next tick (or same tick, since `poll`
calls the enforcer after applying replies).

The no-spam property is preserved: an unanswered prompt keeps `open_dim ==
prompted_dimension`, so it is not resent (protects the intent of
`unconfirmed_hold_is_not_reprompted`).

### 4. Fan-out guards

In `src/state.rs`, make the payee mutators no-op on an already-settled member so a
group-wide payee reply cannot corrupt a settled sibling:

- `create_pending`: if `entry.payee_settled()` → no-op (do not set `create_new`).
- `remap_pending`: if `entry.payee_settled()` → no-op.
- `confirm_pending`: already safe (only copies a `suggestion` into `resolve_to`);
  no change required, but verify under test.

This matches the intent of the existing `heterogeneous_group_payee_confirm_is_safe`
test and extends it to create/remap.

### 5. Auto-heal (free)

Every entry in the current `state.toml` has no `prompted_dimension` → `None`. On the
first `poll`/`run` after deploy, `None != Q` for every non-fully-resolved group → each
gets exactly one correct prompt for its open dimension. The backlog (~45 entries,
grouped by merchant ≈ 25 messages) surfaces itself, sent sequentially; a Telegram 429
just warns and retries next tick. No manual state editing.

### 6. Error handling

- Enforcer send failures: warn and continue (retry next tick) — unchanged from
  today's `send_group_prompts` `Err` arm.
- `edit_message_text` failures on the superseded prompt: warn, do not abort the send
  (preserves `failed_keyboard_tidy_does_not_abort_chaining` intent).
- Persist failure after a send: warn ("may re-prompt next run") — unchanged.

## Testing (TDD)

Unit:
- `Q(group)` computation across homogeneous and mixed groups.
- `needs_prompt`: fresh hold → prompt once; unanswered same-dimension → NOT resent
  (no-spam); payee just settled (open_dim=Category, prompted_dimension=Payee) →
  needs category prompt; new payee-open member in a category-phase group → group
  question reverts to Payee → needs payee prompt.
- Fan-out guards: `create_pending`/`remap_pending` no-op on a payee-settled member;
  `confirm_pending` still safe.

Integration:
- Reproduce the `MR FERNANDO` heterogeneous strand: payee prompt → payee reply
  fans out → next tick category prompt → category reply → import.
- Auto-heal: entries constructed with `prompted_dimension = None` and a stale
  `message_id` get exactly one correct prompt on the next `poll`/`run`.

Expected churn: the `poll` tests exercising `chain_after_payee_settle` /
`send_chained_category_prompts`
(`payee_confirm_chains_category_prompt_immediately`,
`chaining_is_scoped_to_groups_settled_this_tick`,
`empty_category_cache_falls_back_to_bare_clear`,
`failed_keyboard_tidy_does_not_abort_chaining`,
`send_chained_only_sends_for_listed_merchants`) are rewritten against
`ensure_prompts`. The same-tick category-prompt behavior is preserved.

## Rollout

1. Implement + full test suite green (`cargo test`, `cargo clippy --all-targets --
   -D warnings`, `cargo fmt --all -- --check`).
2. Merge to `main` → CI builds the GHCR image.
3. Redeploy (requires Monzo re-auth per existing operational note).
4. On the first `poll`/`run`, verify the backlog surfaces as Telegram prompts and
   drains as answered; confirm previously-missing transactions land in Actual.
