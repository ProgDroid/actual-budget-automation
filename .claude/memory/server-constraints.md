---
name: server-constraints
description: "User's deployment target for aba — memory-constrained headless Linux server, Windows dev box, GHCR pull model"
metadata: 
  node_type: memory
  type: user
  originSessionId: 74b95b3b-3843-40a0-8301-6f93066ab4f6
---

The user runs aba on a **memory-constrained headless Linux server** (only web UIs exposed, no browser). **RAM is the #1 constraint, CPU the #2 constraint.** They develop on a **Windows** machine and deploy by **publishing a Docker image to GHCR and pulling it on the server**, running it on the same Docker network as a self-hosted Actual Budget instance.

**Why:** This ranking directly drove a real architecture decision — aba runs as a **one-shot container** (cron/systemd-triggered, 0 RAM between runs) rather than a resident scheduler daemon, talking to Actual on the shared Docker network. **CORRECTION (2026-06-02):** the original "plain HTTP, no TLS/cert surface" assumption proved FALSE at first deploy — the self-hosted Actual server actually enforces **HTTPS with a self-signed cert** on 5006, so aba must trust the cert and use a SAN-matching URL; see [[actual-connection-tls]]. Browser-only flows (Monzo OAuth bootstrap) must run on the Windows dev box, not the server.

**How to apply:** For future infra/feature choices on this project, prefer designs that idle at zero memory and avoid always-on processes; default to GHCR-published images the user pulls, not local Docker builds (they have no local Docker). When a feature needs a browser or interactivity, assume it cannot run unattended on the server. The non-root container runs as uid 1000, so any mounted volume (e.g. `/data`) must be writable by uid 1000. Tooling gotchas for this stack are in [[rust-harness-gotchas]].

**CI / image pipeline (the GHCR image source):** the only workflow is `.github/workflows/release.yml` — three jobs: `test` (fmt/clippy incl. `--no-default-features`/`cargo test --all`), `audit` (`cargo audit` + `npm audit` in `actual-shim`), then `image` (builds + pushes to `ghcr.io/<repo>`, gated on test+audit). **It triggers ONLY on `push` to `main` and `v*` tags — there is NO `pull_request` trigger.** Consequence (cost me reasoning 2026-06-11): a CI/workflow change cannot be validated on a feature branch or PR; it only runs once it lands on `main`. So land workflow edits on `main` and watch the run (`gh run watch`). As of 2026-06-11 all actions are on their Node-24 majors (`actions/checkout@v6`, `setup-node@v6`, `docker/setup-buildx@v4`, `login@v4`, `metadata@v6`, `build-push@v7`) — bumped ahead of GitHub forcing Node 20 actions onto Node 24 on 2026-06-16.
