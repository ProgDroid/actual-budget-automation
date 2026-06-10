//! Shared test fixtures for the pipeline integration + unit tests.
#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use tempfile::TempDir;

use crate::actual::ActualClient;
use crate::banks::{BankError, BankSource, RawTransaction, TransferTarget};
use crate::claude::{Classification, ClaudeMatcher, Confidence};
use crate::config::Config;
use crate::notify::Notifier;
use crate::state::{PendingReview, State};
use crate::telegram::{ConfirmChannel, Prompt, Reply};

pub fn tx(id: &str, merchant: &str, amount: i64) -> RawTransaction {
    RawTransaction {
        external_id: id.into(),
        date: NaiveDate::from_ymd_opt(2026, 5, 30).unwrap(),
        amount_minor: amount,
        raw_merchant: merchant.into(),
        source_monzo_id: "acc_1".into(),
        transfer: None,
    }
}

/// Test double: returns scripted replies once, records sent prompts and edits.
pub struct FakeChannel {
    pub scripted: Mutex<Vec<Reply>>,
    pub offset: i64,
    pub sent: Mutex<Vec<Prompt>>,
    pub next_id: Mutex<i64>,
    pub edited: Mutex<Vec<(i64, String)>>,
    pub edit_should_fail: bool,
}
impl FakeChannel {
    pub fn new(scripted: Vec<Reply>, offset: i64) -> Self {
        Self {
            scripted: Mutex::new(scripted),
            offset,
            sent: Mutex::new(Vec::new()),
            next_id: Mutex::new(1000),
            edited: Mutex::new(Vec::new()),
            edit_should_fail: false,
        }
    }

    /// Variant whose `edit_message_text` always errors, to prove the run
    /// continues regardless.
    pub fn with_failing_edit(mut self) -> Self {
        self.edit_should_fail = true;
        self
    }

    /// Number of `edit_message_text` calls recorded so far.
    pub fn edits(&self) -> usize {
        self.edited.lock().unwrap().len()
    }
}
#[async_trait]
impl ConfirmChannel for FakeChannel {
    async fn poll_replies(
        &self,
        _since: i64,
        _msg_map: &BTreeMap<i64, String>,
    ) -> anyhow::Result<(Vec<Reply>, i64)> {
        Ok((
            std::mem::take(&mut *self.scripted.lock().unwrap()),
            self.offset,
        ))
    }
    async fn send_prompt(&self, prompt: &Prompt) -> anyhow::Result<i64> {
        self.sent.lock().unwrap().push(prompt.clone());
        let mut id = self.next_id.lock().unwrap();
        *id += 1;
        Ok(*id)
    }
    async fn edit_message_text(&self, message_id: i64, text: &str) -> anyhow::Result<()> {
        self.edited
            .lock()
            .unwrap()
            .push((message_id, text.to_string()));
        if self.edit_should_fail {
            return Err(anyhow::anyhow!("edit failed (test)"));
        }
        Ok(())
    }
}

pub fn cls(
    raw: &str,
    payee: Option<&str>,
    pc: Confidence,
    cat: Option<&str>,
    cc: Confidence,
) -> Classification {
    Classification {
        raw: raw.into(),
        payee: payee.map(Into::into),
        payee_confidence: pc,
        category: cat.map(Into::into),
        category_confidence: cc,
    }
}

pub fn held(external_id: &str, raw_merchant: &str) -> PendingReview {
    PendingReview {
        external_id: external_id.into(),
        source_monzo_id: "acc_1".into(),
        raw_merchant: raw_merchant.into(),
        amount: -100,
        date: "2026-06-01".into(),
        resolve_to: String::new(),
        create_new: false,
        suggestion: Some("Transport for London".into()),
        confidence: Some("medium".into()),
        category_suggestion: None,
        category_confidence: None,
        resolve_category_to: String::new(),
        category_resolved: true,
        telegram_message_id: None,
    }
}

pub struct FakeBank {
    pub txs: Vec<RawTransaction>,
}
#[async_trait]
impl BankSource for FakeBank {
    fn name(&self) -> &'static str {
        "fake"
    }
    async fn ensure_auth(&mut self, _s: &mut State) -> Result<(), BankError> {
        Ok(())
    }
    async fn fetch_since(&self, _since: DateTime<Utc>) -> Result<Vec<RawTransaction>, BankError> {
        Ok(self.txs.clone())
    }
}

pub struct FakeClaude {
    pub out: Vec<Classification>,
}
#[async_trait]
impl ClaudeMatcher for FakeClaude {
    async fn classify(
        &self,
        _raw: &[String],
        _p: &[String],
        _c: &[String],
    ) -> anyhow::Result<Vec<Classification>> {
        Ok(self.out.clone())
    }
}

pub struct NullNotifier;
#[async_trait]
impl Notifier for NullNotifier {
    async fn notify(&self, _t: &str, _b: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Notifier double that records every notify(title, body) call.
pub struct RecordingNotifier {
    pub calls: Mutex<Vec<(String, String)>>,
}
impl RecordingNotifier {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
}
#[async_trait]
impl Notifier for RecordingNotifier {
    async fn notify(&self, t: &str, b: &str) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((t.to_string(), b.to_string()));
        Ok(())
    }
}

/// Bank whose `ensure_auth` always reports the 90-day re-auth condition.
pub struct AuthExpiredBank;
#[async_trait]
impl BankSource for AuthExpiredBank {
    fn name(&self) -> &'static str {
        "fake"
    }
    async fn ensure_auth(&mut self, _s: &mut State) -> Result<(), BankError> {
        Err(BankError::AuthExpired("test".into()))
    }
    async fn fetch_since(&self, _since: DateTime<Utc>) -> Result<Vec<RawTransaction>, BankError> {
        panic!("fetch_since must not be called after AuthExpired");
    }
}

/// Bank that simulates Monzo's refresh-token rotation during `ensure_auth`.
pub struct RotatingBank {
    pub rotate_to: String,
}
#[async_trait]
impl BankSource for RotatingBank {
    fn name(&self) -> &'static str {
        "fake"
    }
    async fn ensure_auth(&mut self, s: &mut State) -> Result<(), BankError> {
        s.monzo.refresh_token = self.rotate_to.clone();
        Ok(())
    }
    async fn fetch_since(&self, _since: DateTime<Utc>) -> Result<Vec<RawTransaction>, BankError> {
        Ok(vec![])
    }
}

/// Build an `ActualClient` backed by a fake node shim in `dir`.
/// `get_payees` returns `[{p_cafe, Local Cafe}]`, `get_scheduled` is empty,
/// `create_payee` returns `{id: p_new}`, and `import_transactions` appends
/// the imported rows to `<dir>/imports.log` (one JSON array per call).
pub fn fake_actual(dir: &TempDir) -> ActualClient {
    let script = r"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  const fs=require('fs'), path=require('path');
  if(req.op==='get_payees'){ out({ok:true,data:[{id:'p_cafe',name:'Local Cafe'}]}); }
  else if(req.op==='get_scheduled'){ out({ok:true,data:[]}); }
  else if(req.op==='get_categories'){ out({ok:true,data:[{id:'c_groc',name:'Groceries'},{id:'c_din',name:'Dining'}]}); }
  else if(req.op==='create_payee'){ out({ok:true,data:{id:'p_new'}}); }
  else if(req.op==='import_transactions'){
    fs.appendFileSync(path.join(__dirname,'imports.log'), JSON.stringify(req.transactions)+'\n');
    out({ok:true,data:{added:req.transactions.length,updated:0}});
  } else { out({ok:false,error:'unknown op'}); }
});
";
    std::fs::write(dir.path().join("index.js"), script).unwrap();
    ActualClient::new(
        dir.path().to_path_buf(),
        false,
        "http://t".into(),
        "pw".into(),
        "sync".into(),
    )
}

/// Like `fake_actual` but `import_transactions` fails, and `create_payee`
/// records that it was called (to `payees.log`) before returning an id.
pub fn fake_actual_failing_import(dir: &TempDir) -> ActualClient {
    let script = r"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  const fs=require('fs'), path=require('path');
  if(req.op==='get_payees'){ out({ok:true,data:[{id:'p_cafe',name:'Local Cafe'}]}); }
  else if(req.op==='get_scheduled'){ out({ok:true,data:[]}); }
  else if(req.op==='get_categories'){ out({ok:true,data:[{id:'c_groc',name:'Groceries'},{id:'c_din',name:'Dining'}]}); }
  else if(req.op==='create_payee'){
    fs.appendFileSync(path.join(__dirname,'payees.log'), 'called\n');
    out({ok:true,data:{id:'p_new'}});
  }
  else if(req.op==='import_transactions'){ out({ok:false,error:'boom'}); }
  else { out({ok:false,error:'unknown op'}); }
});
";
    std::fs::write(dir.path().join("index.js"), script).unwrap();
    ActualClient::new(
        dir.path().to_path_buf(),
        false,
        "http://t".into(),
        "pw".into(),
        "sync".into(),
    )
}

/// Like `fake_actual` but `get_payees` fails — simulates the Actual server
/// being unreachable *after* a successful Monzo refresh (the exact path that
/// silently burned rotated refresh tokens).
pub fn fake_actual_failing_get_payees(dir: &TempDir) -> ActualClient {
    let script = r"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  if(req.op==='get_payees'){ out({ok:false,error:'network-failure'}); }
  else { out({ok:false,error:'unknown op'}); }
});
";
    std::fs::write(dir.path().join("index.js"), script).unwrap();
    ActualClient::new(
        dir.path().to_path_buf(),
        false,
        "http://t".into(),
        "pw".into(),
        "sync".into(),
    )
}

/// Like `fake_actual` but exposes TWO payees (Local Cafe + Bills Co) and
/// records each `import_transactions` call as one JSON line
/// `{"account_id":..,"transactions":[..]}` in `<dir>/routed.log`, so a test
/// can assert which Actual account each transaction was routed to.
pub fn fake_actual_routing(dir: &TempDir) -> ActualClient {
    let script = r"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  const fs=require('fs'), path=require('path');
  if(req.op==='get_payees'){ out({ok:true,data:[{id:'p_cafe',name:'Local Cafe'},{id:'p_bills',name:'Bills Co'}]}); }
  else if(req.op==='get_scheduled'){ out({ok:true,data:[]}); }
  else if(req.op==='get_categories'){ out({ok:true,data:[{id:'c_groc',name:'Groceries'},{id:'c_din',name:'Dining'}]}); }
  else if(req.op==='create_payee'){ out({ok:true,data:{id:'p_new'}}); }
  else if(req.op==='import_transactions'){
    fs.appendFileSync(path.join(__dirname,'routed.log'), JSON.stringify({account_id:req.account_id,transactions:req.transactions})+'\n');
    out({ok:true,data:{added:req.transactions.length,updated:0}});
  } else { out({ok:false,error:'unknown op'}); }
});
";
    std::fs::write(dir.path().join("index.js"), script).unwrap();
    ActualClient::new(
        dir.path().to_path_buf(),
        false,
        "http://t".into(),
        "pw".into(),
        "sync".into(),
    )
}

/// Like `fake_actual_routing` but `get_payees` also returns transfer payees:
/// `{p_xfer_pot, transfer_acct:'act-pot'}` and `{p_xfer_cur, transfer_acct:'act-cur'}`.
/// `import_transactions` logs `{account_id, transactions}` lines to `<dir>/routed.log`.
pub fn fake_actual_transfers(dir: &TempDir) -> ActualClient {
    let script = r"
let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{
  const req=JSON.parse(b); const out=o=>process.stdout.write(JSON.stringify(o));
  const fs=require('fs'), path=require('path');
  if(req.op==='get_payees'){ out({ok:true,data:[{id:'p_cafe',name:'Local Cafe'},{id:'p_xfer_pot',name:'Transfer: Savings',transfer_acct:'act-pot'},{id:'p_xfer_cur',name:'Transfer: Current',transfer_acct:'act-cur'},{id:'p_xfer_flex',name:'Transfer: Flex',transfer_acct:'act-flex'}]}); }
  else if(req.op==='get_scheduled'){ out({ok:true,data:[]}); }
  else if(req.op==='get_categories'){ out({ok:true,data:[{id:'c_groc',name:'Groceries'}]}); }
  else if(req.op==='create_payee'){ out({ok:true,data:{id:'p_new'}}); }
  else if(req.op==='import_transactions'){
    fs.appendFileSync(path.join(__dirname,'routed.log'), JSON.stringify({account_id:req.account_id,transactions:req.transactions})+'\n');
    out({ok:true,data:{added:req.transactions.length,updated:0}});
  } else { out({ok:false,error:'unknown op'}); }
});
";
    std::fs::write(dir.path().join("index.js"), script).unwrap();
    ActualClient::new(
        dir.path().to_path_buf(),
        false,
        "http://t".into(),
        "pw".into(),
        "sync".into(),
    )
}

pub fn cfg_at(state_path: &std::path::Path) -> Config {
    Config::from_getter(|k| match k {
        "ANTHROPIC_API_KEY" => Some("ak".into()),
        "MONZO_CLIENT_ID" => Some("c".into()),
        "MONZO_CLIENT_SECRET" => Some("s".into()),
        "MONZO_REDIRECT_URI" => Some("http://localhost/cb".into()),
        "APPRISE_URL" => Some("http://a/notify".into()),
        "ACTUAL_SERVER_URL" => Some("http://actual".into()),
        "ACTUAL_PASSWORD" => Some("pw".into()),
        "ACTUAL_SYNC_ID" => Some("sync".into()),
        "ABA_STATE_PATH" => Some(state_path.to_string_lossy().to_string()),
        _ => None,
    })
    .unwrap()
}

/// Build a pot-deposit transfer row: negative amount, source `acc_cur`,
/// `transfer: Some(Pot("pot_abc"))`.
pub fn pot_deposit(id: &str) -> RawTransaction {
    RawTransaction {
        external_id: id.into(),
        date: NaiveDate::from_ymd_opt(2026, 5, 30).unwrap(),
        amount_minor: -2000,
        raw_merchant: "Pot deposit".into(),
        source_monzo_id: "acc_cur".into(),
        transfer: Some(TransferTarget::Pot("pot_abc".into())),
    }
}
