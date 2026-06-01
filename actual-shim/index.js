'use strict';

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
    case 'get_payees': {
      const payees = await api.getPayees();
      return payees.map((p) => ({ id: p.id, name: p.name }));
    }
    case 'get_scheduled': {
      const { data } = await api.runQuery(
        api.q('schedules').select(['id', 'name', '_date', '_amount', '_payee'])
      );
      return data.map((s) => ({
        date: typeof s._date === 'string' ? s._date : null,
        amount: typeof s._amount === 'number' ? s._amount : (s._amount && s._amount.num1) || 0,
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
    await api.init({ dataDir: DATA_DIR, serverURL: SERVER_URL, password: PASSWORD });
    await api.downloadBudget(SYNC_ID);
    const data = await handle(req);
    response = { ok: true, data };
  } catch (e) {
    response = { ok: false, error: String((e && e.message) || e) };
  } finally {
    try {
      await api.shutdown();
    } catch (_) {
      /* ignore shutdown errors */
    }
  }
  process.stdout.write(JSON.stringify(response));
  process.exit(response.ok ? 0 : 1);
})();
