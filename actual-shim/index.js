'use strict';

// stdout is this shim's single-message JSON protocol channel back to the Rust
// caller. @actual-app/api logs verbose "[Breadcrumb]" lines via console.log
// (verboseMode defaults to true) and may otherwise write to stdout, which would
// corrupt the response ("shim output not valid JSON: [Breadcrumb] ..."). The
// "Closing budget" breadcrumb fires inside api.shutdown() on every call, so this
// affects success responses too, not just errors. Reserve the real stdout for the
// final JSON envelope only and route all other writes (console.* and any direct
// stdout writes) to stderr, which the Rust side already captures as diagnostics.
const realStdoutWrite = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);

const fs = require('fs');
const api = require('@actual-app/api');

const READ_ONLY = process.argv.includes('--read-only');

const SERVER_URL = process.env.ACTUAL_SERVER_URL;
const PASSWORD = process.env.ACTUAL_PASSWORD;
const SYNC_ID = process.env.ACTUAL_SYNC_ID;
const DATA_DIR = process.env.ACTUAL_DATA_DIR || '/data/actual-cache';

function readStdin() {
  return new Promise((resolve, reject) => {
    let buf = '';
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', (c) => (buf += c));
    process.stdin.on('end', () => resolve(buf));
    process.stdin.on('error', reject);
  });
}

async function handle(req) {
  switch (req.op) {
    case 'get_accounts': {
      const accounts = await api.getAccounts();
      return accounts.map((a) => ({ id: a.id, name: a.name }));
    }
    case 'get_payees': {
      const payees = await api.getPayees();
      return payees.map((p) => ({
        id: p.id,
        name: p.name,
        transfer_acct: p.transfer_acct ?? null,
      }));
    }
    case 'get_categories': {
      const cats = await api.getCategories();
      // Real categories carry a group_id; category *groups* do not — exclude
      // groups so only assignable categories are offered.
      return cats
        .filter((c) => c && c.group_id)
        .map((c) => ({ id: c.id, name: c.name }));
    }
    case 'get_scheduled': {
      const { data } = await api.runQuery(
        api.q('schedules').select(['id', 'name', '_date', '_amount', '_payee'])
      );
      return data.map((s) => ({
        date: typeof s._date === 'string' ? s._date : null,
        amount: typeof s._amount === 'number' ? s._amount : null,
        payee_id: s._payee || null,
      }));
    }
    case 'import_transactions': {
      if (READ_ONLY) throw new Error('refused: shim is in --read-only mode');
      const result = await api.importTransactions(req.account_id, req.transactions);
      return { added: result.added.length, updated: result.updated.length };
    }
    case 'create_payee': {
      if (READ_ONLY) throw new Error('refused: shim is in --read-only mode');
      const id = await api.createPayee({ name: req.name });
      return { id };
    }
    default:
      throw new Error(`unknown op: ${req.op}`);
  }
}

(async () => {
  let response;
  try {
    const raw = await readStdin();
    const req = JSON.parse(raw);
    // Actual's init() does not create dataDir; it scandir's it and throws ENOENT
    // if missing. The bind-mounted /data exists (writable by uid 1000) but the
    // actual-cache subdir is never pre-created by the deploy, so ensure it here.
    fs.mkdirSync(DATA_DIR, { recursive: true });
    await api.init({
      dataDir: DATA_DIR,
      serverURL: SERVER_URL,
      password: PASSWORD,
      verbose: false,
    });
    await api.downloadBudget(SYNC_ID);
    const data = await handle(req);
    response = { ok: true, data };
  } catch (e) {
    let msg = String((e && e.message) || e);
    if (PASSWORD) msg = msg.split(PASSWORD).join('***');
    response = { ok: false, error: msg };
  } finally {
    try {
      await api.shutdown();
    } catch (_) {
      /* ignore shutdown errors */
    }
  }
  realStdoutWrite(JSON.stringify(response));
  process.exit(response.ok ? 0 : 1);
})();
