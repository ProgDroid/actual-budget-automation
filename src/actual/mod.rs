//! Actual Budget client via Node shim.

pub mod shim_protocol;

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, anyhow};
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use shim_protocol::{ImportTxn, Payee, ScheduleRow, ShimRequest, ShimResponse};

/// Spawns the Node shim once per operation, exchanging JSON over stdin/stdout.
pub struct ActualClient {
    /// Program to run (production: "node").
    program: PathBuf,
    /// Leading args (production: the shim's index.js path).
    script_args: Vec<String>,
    /// Working dir (also where `node_modules` lives in production).
    cwd: PathBuf,
    read_only: bool,
    server_url: String,
    password: String,
    sync_id: String,
}

impl ActualClient {
    /// Production constructor: runs `node <shim_dir>/index.js` with `cwd = shim_dir`.
    pub fn new(
        shim_dir: PathBuf,
        read_only: bool,
        server_url: String,
        password: String,
        sync_id: String,
    ) -> Self {
        let index = shim_dir.join("index.js");
        Self {
            program: PathBuf::from("node"),
            script_args: vec![index.to_string_lossy().into_owned()],
            cwd: shim_dir,
            read_only,
            server_url,
            password,
            sync_id,
        }
    }

    async fn call<T: DeserializeOwned>(&self, req: &ShimRequest) -> anyhow::Result<T> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.script_args)
            .current_dir(&self.cwd)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.env("ACTUAL_SERVER_URL", &self.server_url)
            .env("ACTUAL_PASSWORD", &self.password)
            .env("ACTUAL_SYNC_ID", &self.sync_id);
        if self.read_only {
            cmd.arg("--read-only");
        }
        let mut child = cmd.spawn().context("failed to spawn shim")?;
        {
            let mut stdin = child.stdin.take().context("no stdin on shim")?;
            let payload = serde_json::to_vec(req)?;
            stdin.write_all(&payload).await?;
            stdin.shutdown().await?;
        }
        let output =
            match tokio::time::timeout(std::time::Duration::from_mins(2), child.wait_with_output())
                .await
            {
                Ok(res) => res.context("shim wait failed")?,
                // Future dropped here -> child dropped -> killed via kill_on_drop.
                Err(_) => return Err(anyhow!("shim timed out after 120s")),
            };
        if !output.stderr.is_empty() {
            let mut err_text = String::from_utf8_lossy(&output.stderr).into_owned();
            if !self.password.is_empty() {
                err_text = err_text.replace(&self.password, "***");
            }
            if output.status.success() {
                tracing::debug!("shim stderr: {err_text}");
            } else {
                tracing::error!("shim exited with {:?}: {err_text}", output.status.code());
            }
        }
        let parsed: ShimResponse<T> =
            serde_json::from_slice(&output.stdout).with_context(|| {
                format!(
                    "shim output not valid JSON: {}",
                    String::from_utf8_lossy(&output.stdout)
                )
            })?;
        match parsed {
            ShimResponse::Ok { data } => Ok(data),
            ShimResponse::Err { error } => {
                let safe = if self.password.is_empty() {
                    error
                } else {
                    error.replace(&self.password, "***")
                };
                Err(anyhow!("shim error: {safe}"))
            }
        }
    }

    /// # Errors
    /// Returns an error if the shim fails to spawn, exits non-zero, or returns invalid JSON.
    pub async fn get_payees(&self) -> anyhow::Result<Vec<Payee>> {
        self.call(&ShimRequest::GetPayees).await
    }

    /// # Errors
    /// Returns an error if the shim fails to spawn, exits non-zero, or returns invalid JSON.
    pub async fn get_scheduled(&self) -> anyhow::Result<Vec<ScheduleRow>> {
        self.call(&ShimRequest::GetScheduled).await
    }

    /// # Errors
    /// Returns an error if the shim fails to spawn, exits non-zero, or returns invalid JSON.
    pub async fn import_transactions(
        &self,
        account_id: String,
        transactions: Vec<ImportTxn>,
    ) -> anyhow::Result<ImportSummary> {
        self.call(&ShimRequest::ImportTransactions {
            account_id,
            transactions,
        })
        .await
    }

    /// # Errors
    /// Returns an error if the shim fails to spawn, exits non-zero, or returns invalid JSON.
    pub async fn create_payee(&self, name: String) -> anyhow::Result<CreatedPayee> {
        self.call(&ShimRequest::CreatePayee { name }).await
    }
}

#[derive(Debug, serde::Deserialize, PartialEq, Eq)]
pub struct ImportSummary {
    pub added: usize,
    pub updated: usize,
}

#[derive(Debug, serde::Deserialize, PartialEq, Eq)]
pub struct CreatedPayee {
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn get_payees_parses_data() {
        let dir = tempdir().unwrap();
        // Fake shim: ignore stdin content, print a canned ok envelope.
        let fake = r"let b='';process.stdin.on('data',c=>b+=c);process.stdin.on('end',()=>{process.stdout.write(JSON.stringify({ok:true,data:[{id:'p1',name:'Amazon'}]}));});";
        std::fs::write(dir.path().join("index.js"), fake).unwrap();

        let client = ActualClient::new(
            dir.path().to_path_buf(),
            false,
            "http://test".into(),
            "pw".into(),
            "sync".into(),
        );
        let payees = client.get_payees().await.unwrap();
        assert_eq!(
            payees,
            vec![Payee {
                id: "p1".into(),
                name: "Amazon".into()
            }]
        );
    }
}
