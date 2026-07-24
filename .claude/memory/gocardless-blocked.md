---
name: gocardless-blocked
description: GoCardless/Nordigen (Phase 2 backlog
metadata: 
  node_type: memory
  type: project
  originSessionId: b512701a-764f-41cd-aa84-307a3742e19d
---

As of 2026-06-02, **GoCardless / Nordigen is not accepting new registrations**, so Phase 2 backlog item #2 (GoCardless bank source for Santander + other UK banks) is **blocked** and an alternative open-banking aggregator will be needed for non-Monzo UK banks.

**Why:** #2 was the planned path to broaden bank coverage beyond Monzo and was the reason #7's `encode_callback` `debug_assert!(len<=64)` fix was sequenced first (longer GoCardless IDs). With #2 blocked, that sequencing pressure is relieved — the encoder fix is still worth doing but is no longer gating.

**How to apply:** Don't start #2 against GoCardless. When bank coverage comes back up, research alternative aggregators (e.g. TrueLayer, Plaid, Tink, Yapily) that still onboard new developers. See [[phase2-progress]].
