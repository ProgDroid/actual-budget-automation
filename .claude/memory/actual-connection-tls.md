---
name: actual-connection-tls
description: "aba→Actual connection reality — Actual serves HTTPS with a self-signed cert; URL host must match the cert SAN + NODE_EXTRA_CA_CERTS; 'network-failure' is a catch-all"
metadata: 
  node_type: memory
  type: project
  originSessionId: 94da65b7-91ab-40e1-aed7-bd649ba7cfea
---

The self-hosted Actual server in the user's deployment serves **HTTPS with a self-signed certificate** on `:5006` — NOT plain HTTP, contrary to the original design assumption (corrected in [[server-constraints]]). Getting the `aba` one-shot container to connect required, in this exact order (each step revealed the next, layer by layer):

1. **Correct Docker service name.** The Actual container is NOT named `actual`; the default `ACTUAL_SERVER_URL=http://actual:5006` yields `ENOTFOUND`. Use the real compose service name.
2. **`https://`, not `http://`.** Actual speaks TLS on 5006; plain HTTP gets `UND_ERR_SOCKET` ("other side closed" — undici/Node's fetch sees the TLS listener drop the plaintext request).
3. **`NODE_EXTRA_CA_CERTS`.** The self-signed leaf gives `DEPTH_ZERO_SELF_SIGNED_CERT`. Mount the cert into the aba container and set `NODE_EXTRA_CA_CERTS=/path/cert.pem`. The Rust client spawns `node` WITHOUT `env_clear()` (`src/actual/mod.rs`), so the shim's Node process inherits this from the container env. (`NODE_TLS_REJECT_UNAUTHORIZED=0` is the quick, less-safe alternative — disables verification for that process only; low risk on a private bridge but verification is off.)
4. **URL host must be in the cert SAN.** Connecting by a name absent from the cert's Subject Alternative Names gives `ERR_TLS_CERT_ALTNAME_INVALID` even with the CA now trusted. Set `ACTUAL_SERVER_URL` to a hostname listed in the cert's SAN (or regenerate the cert with the service name in its SAN).

**Why:** `@actual-app/api` collapses ALL of DNS/TLS/socket failures into one unhelpful message: `Authentication failed: network-failure`, surfaced by aba as `shim error: Authentication failed: network-failure`. The password is never even checked — it is purely a transport failure. So "network-failure" at the `get_payees`/`init` step means **connectivity, not credentials**.

**How to apply:** When a run fails at the Actual step with `network-failure`, probe the transport layer-by-layer from inside the aba container's network using Node's built-in `fetch`/`tls.connect` (the node:slim image has no curl/wget): resolve name → http → https → CA trust → SAN match. It is deployment config, not a code bug. The Monzo refresh step runs BEFORE `get_payees`, so an Actual failure here can have a side effect on the Monzo token — see [[monzo-token-rotation-invariant]] and [[server-constraints]].
