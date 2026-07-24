---
name: actual-shim-stdout-and-datadir
description: "First-deploy shim failures — @actual-app/api verbose breadcrumbs corrupt the stdout JSON channel, and init() does not create dataDir."
metadata: 
  node_type: memory
  type: project
  originSessionId: 6824f1bb-152a-4e0d-b392-be25b181e1d0
---

First real deploy dry-run failed with `shim output not valid JSON: [Breadcrumb] { message: 'Closing budget' }` wrapping `ENOENT ... scandir '/data/actual-cache'`. Two stacked root causes in `actual-shim/index.js`:

1. **`@actual-app/api` v26 defaults `verboseMode = true`** (`dist/index.js:3311`). Breadcrumbs go `captureBreadcrumb → logger.info → console.log → stdout`. The "Closing budget" breadcrumb fires inside `api.shutdown()` (run in `finally` on EVERY call), so it corrupts success responses too — universal blocker, not just an error path. Unit test missed it (fake shim never loads the real API).
2. **`init()` does not create `dataDir`** — it `scandir`s it and throws ENOENT. Deploy chowns `/srv/aba`→`/data` but never creates the `actual-cache` subdir.

**Fix (shim, source-side):** reserve real stdout for the JSON envelope only — capture `realStdoutWrite` then `process.stdout.write = process.stderr.write.bind(...)` so all console/lib writes go to stderr (Rust already logs it); pass `verbose:false` to `init()`; `fs.mkdirSync(DATA_DIR, {recursive:true})` before init. Rust parser unchanged — contract is "shim writes only JSON to stdout." Verified locally: shim syntax OK, stdout isolation proven, build+clippy+86 tests green. NOT yet verified end-to-end against the live server (needs image rebuild + redeploy). See [[phase2-progress]], [[fetch-failloud-invariant]].
