# Categories — Claude-Classified `category_id` — Design (Phase 2)

Date: 2026-06-03
Status: Approved (brainstorming) — pending written-spec review

## Background

This is **Phase 2 of four** agreed features. The full set, in build order:

1. `account_id` split + multi-account routing — **DONE** (Phase 1, on `main`).
2. **Categories (Claude-classified `category_id`)** ← *this spec*
3. Transfers (incl. Monzo pots, which are transfers in the parent feed).
4. Telegram prompts grouped per merchant.

Each phase ships independently. This phase extends the existing payee
classification pipeline with a second dimension; it does not introduce a new
subsystem.

## Problem

Imported transactions land in Actual **uncategorised**. Actual only
auto-categorises if the user has configured rules, and the user has none — so
every transaction requires manual categorisation in the Actual UI.

We already classify each merchant's **payee** with Claude (the `ClaudeMatcher`
trait → `report_matches` tool call → confidence-gated auto-import vs. Telegram
review, with a merchant-keyed cache). The category is a naturally correlated
second decision (the payee usually implies the category), so we extend that same
pipeline rather than build a parallel one.

## Decisions (locked during brainstorming)

1. **Categorisation goal:** confident classifications auto-import; uncertain ones
   route to the existing Telegram review loop (same pattern as payees).
2. **Source of truth:** Claude classifies each merchant into one of the user's
   **existing Actual categories** (full taxonomy granularity). Monzo's own
   coarse `category` field is *not* used in this phase.
3. **Review unit:** a transaction is held if **either** payee **or** category is
   uncertain (preserves today's atomic import-or-hold model; correct on first
   import).
4. **Review interaction:** **sequential single-dimension** Telegram prompts —
   resolve payee first (existing prompt), then category (new prompt). Each prompt
   is one dimension via free-text reply, reusing the current mechanism with no
   reply-disambiguation. A both-uncertain txn takes two round-trips.
5. **One combined Claude call** returns both payee and category per merchant
   (correlated decisions; halves token cost; reuses the `report_matches` shape).

## Scope

**In scope**

- Fetch the user's Actual categories.
- Extend Claude classification to return a category + category-confidence per
  merchant, in the same call as the payee.
- Gate auto-import on **both** payee and category being confident *and* resolving
  to a live Actual entity.
- Set the category id on imported transactions.
- A second, category-only cache.
- A sequential category review prompt in the Telegram loop.

**Out of scope (non-goals)**

- **No "create new category" via Telegram.** Actual categories require a category
  *group*; choosing/creating a group mid-review is deferred. To add a category,
  the user creates it in Actual and remaps. (This is the one known ergonomic gap;
  accepted for Phase 2.)
- **Transfer/pot categorisation** stays in Phase 3.
- **Monzo's `category` field** remains unparsed; revisit only if Claude accuracy
  proves insufficient.

## Design

### A. Data model

- **Fetch categories.** New shim op `get_categories` → `api.getCategories()`.
  New `shim_protocol::Category { id, name }`. New
  `ActualClient::get_categories()`. Mirrors `get_payees`/`get_accounts`.
- **`ImportTxn`** gains `category: Option<String>` — the Actual category **id**
  (never a name). Serialised field omitted when `None`; the shim passes it to
  `api.importTransactions`.
- **`PendingReview`** gains:
  - `category_suggestion: Option<String>` — Claude's proposed category name for a
    medium/low-confidence held category (`None` = genuine no-match).
  - `category_confidence: Option<String>` — `"medium" | "low"` when this was a
    confidence-held category proposal.
  - `resolve_category_to: String` — set to an existing category name to map to it
    (empty = unset).
  - A per-dimension "settled" marker so `poll` knows which prompt is next. The
    payee dimension is considered settled when `resolve_to`/`create_new` is set
    (today's behaviour); the category dimension needs its own explicit flag
    (e.g. `category_resolved: bool`) because "skip → import uncategorised" is a
    valid settled state distinct from "unset".

  All new fields are `#[serde(default)]` so existing on-disk state and held
  entries deserialise unchanged.

### B. Claude classification (one combined call)

- The matcher signature becomes
  `classify(raw, existing_payees, existing_categories) -> Vec<Classification>`,
  where:

  ```rust
  pub struct Classification {
      pub raw: String,
      pub payee: Option<String>,
      pub payee_confidence: Confidence,
      pub category: Option<String>,
      pub category_confidence: Confidence,
  }
  ```

  `PayeeMatch` grows into / is superseded by `Classification`.
- `SYSTEM_INSTRUCTIONS` and the `report_matches` tool JSON schema gain `category`
  and `category_confidence`, with the same conservative "prefer null over a wrong
  guess; use low confidence when null" rule applied independently to the category.
- The category choice is constrained to the provided `existing_categories` list,
  exactly as payee is constrained to `existing_payees`.

### C. Gating (`apply_matches`)

A transaction **auto-imports iff**:

- payee is confident (`>= autoimport` rank) **and** the payee exists in the live
  Actual payee set, **AND**
- category is confident **and** the category exists in the live Actual category
  set.

This extends the Phase-1 C1 existence validation to the category: a confident
match to a category that no longer exists is **held**, never silently dropped
(which would advance `last_run` and lose the categorisation permanently).

Otherwise the transaction is **held**, carrying both suggestions and per-dimension
confidence; `poll` later prompts for whichever dimension(s) are uncertain.

The gating matrix (P = payee, C = category; ✓ = confident & exists):

| Payee | Category | Outcome |
|-------|----------|---------|
| ✓ | ✓ | auto-import with both |
| ✓ | ✗ | hold; category prompt only |
| ✗ | ✓ | hold; payee prompt only |
| ✗ | ✗ | hold; payee prompt then category prompt |

### D. Caching

- Add `category_cache: BTreeMap<raw_merchant, category_name>` beside the existing
  `payee_cache`.
- A merchant **skips Claude only if both caches hit** *and* both cached values
  still resolve to live Actual entities. A legacy merchant (payee cached, no
  category) falls through to Claude for the category.
- Telegram resolutions update **both** caches (a corrected category is remembered
  per merchant, consistent with Phase-4's planned per-merchant grouping, since
  `payee_cache` already keys on `raw_merchant`).
- `ResolutionPlan` extends so that `needs_claude` includes any merchant missing
  *either* cached dimension; `cached` entries must carry both payee and category.

### E. Telegram review (sequential, single-dimension)

- `poll` drives a small state machine per held entry: resolve **payee first**
  (existing prompt, only sent if the payee is uncertain), then **category**
  (new prompt, only sent if the category is uncertain). The transaction stays
  held until **both** dimensions are settled, then imports.
- The category prompt mirrors the payee prompt's "suggestion in list" flavour:

  > `'TESCO' -> 'Groceries'?  (medium confidence)`
  > `Reply with a different category to remap.`
  > Buttons: **✅ Confirm** / **❌ Skip**

- **Skip = import uncategorised** (category id `None`). This guarantees a held
  transaction can never get permanently stuck on the category step.
- Callback encoding reuses `encode_callback`; the same 64-byte cap and
  keyboard-drop fallback apply (free-text reply still resolves via
  `telegram_message_id`). Category actions need distinct action codes
  (e.g. `cc` = confirm category, `cs` = skip category) so a callback's dimension
  is unambiguous.

### F. Import

- At import, resolve the category **name → id** against the live category set
  (the auto-import path already holds the confident category name; the review
  path holds `resolve_category_to`).
- If the name no longer resolves to a live category, **hold (fail-loud)** rather
  than import miscategorised — identical to the Phase-1 payee C1 safeguard.
- Duplicate category names across groups resolve ambiguously; treat as
  **hold, fail-loud** (mirrors Phase-1 ambiguous-account handling). Note: Actual
  permits same-named categories in different groups, so this is a real edge case.

## Error handling & fail-loud invariants

- No silent drops: every "can't resolve" path (unknown payee, unknown category,
  ambiguous category) **holds** the transaction; `last_run` only advances on a
  clean import. Relates to the fetch/`last_run` fail-loud invariant.
- The combined Claude call failing is a hard error for the whole run (as today);
  it does not partially categorise.
- `get_categories` failure is a hard error (we cannot gate categories without the
  live set).

## Testing

Unit coverage:

- **Classification parsing:** the combined `report_matches` response deserialises
  into `Classification` with both dimensions and both confidences.
- **`apply_matches` gating matrix:** all four P×C cells above, plus the
  exists/not-exists axis for each dimension.
- **Cache:** a merchant with only a payee cache hit still goes to `needs_claude`;
  a both-hit merchant skips Claude; a cached value pointing at a now-missing
  entity is re-classified, not used.
- **Category id resolution:** name→id success; missing name → hold; duplicate
  name across groups → hold.
- **`build_prompt`:** the new category-prompt flavour (text + Confirm/Skip
  buttons + over-64-byte keyboard-drop fallback).
- **`poll` state machine:** payee-then-category sequencing; category-skip →
  import uncategorised; import only fires once both dimensions are settled.
- **`ImportTxn` serialisation:** `category` present when `Some`, omitted when
  `None`.

## Files touched (anticipated)

- `actual-shim/index.js` — `get_categories` op; pass `category` in
  `import_transactions`.
- `src/actual/shim_protocol.rs` — `Category` struct; `ImportTxn.category`;
  `ShimRequest`/`ShimResponse` arms.
- `src/actual/mod.rs` — `get_categories()`.
- `src/claude.rs` — `Classification`; `classify()`; `SYSTEM_INSTRUCTIONS` +
  tool schema; `parse_response`.
- `src/matching.rs` — (if any category-name normalisation/similarity is reused).
- `src/state.rs` — `PendingReview` category fields; `category_cache`.
- `src/run.rs` — `apply_matches` gating; `ResolutionPlan`; `build_prompt`
  category flavour; category id resolution at import.
- `src/poll.rs` — sequential payee→category resolution state machine.
- `RUNBOOK.md` — document category classification + the no-create-new-category
  workaround.
