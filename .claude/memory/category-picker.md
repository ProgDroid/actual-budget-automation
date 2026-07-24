---
name: category-picker
description: "Telegram category prompt is now an inline-button picker; built, merged to main (e302511), pushed — NOT yet redeployed (needs Monzo re-auth)."
metadata: 
  node_type: memory
  type: project
  originSessionId: cdd203a4-4665-44d2-be08-d3e1aadcb15b
---

The Telegram **category** prompt changed from "type a category name" to an inline-button **picker** (payee prompt unchanged; free-text typing still works as a fallback). Built 2026-07-07 via a 6-commit branch, **merged to `main` (HEAD `e302511`) and pushed to origin** (fast-forward; CI `release.yml` triggered → new GHCR image). Spec: `docs/superpowers/specs/2026-07-07-category-picker-design.md`; plan: `docs/superpowers/plans/2026-07-07-category-picker.md`.

**Why:** user found category names complicated/hard to recall exactly; a picker also eliminates the "typed a non-existent category → fail-loud at import" hazard since every button is a live category.

**How to apply:**
- Design crux — category buttons carry `callback_data = "pc:<group_key>:<cat_hash>"` (~36 bytes, under Telegram's 64-byte cap), where `cat_hash = telegram::fnv1a_hex(category_name)` (the FNV primitive extracted from `group_key`). A tap is resolved poll-side by re-hashing `state.actual_category_names` to find the match, then `remap_category`. Hashing the NAME (not a positional index) is deliberate: robust to categories being reordered/added; a deleted category → no match → entry stays held and re-prompts (fail-safe). `Prompt.buttons` is now a rows model `Vec<Vec<(label,data)>>`. Builds on Phase D poll-side chaining — see [[multi-account-routing-phase1]].
- **Deploy gap (operational, not code):** still **NOT redeployed**; going live needs the usual **manual redeploy + Monzo re-auth** — same gate blocking all prior merged work. See [[phase2-progress]].
- Known limitation (accepted, out of scope): resolution is by category NAME, so two same-named categories in different Actual groups resolve to whichever the live list yields first (same ambiguity as typing).
