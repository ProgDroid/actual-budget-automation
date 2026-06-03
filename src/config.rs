//! Configuration from environment and CLI.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};

pub const DEFAULT_CLAUDE_MODEL: &str = "claude-sonnet-4-6";
pub const DATE_TOLERANCE_DAYS: i64 = 3;
pub const PAYEE_THRESHOLD: f64 = 0.85;

#[derive(Debug, Parser)]
#[command(name = "aba", about = "Automated Actual Budget transaction import")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fetch, match, and import transactions.
    Run {
        /// Do everything except write to Actual; print what would happen.
        #[arg(long)]
        dry_run: bool,
    },
    /// One-time interactive Monzo OAuth bootstrap (run on a machine with a browser).
    Auth {
        #[command(subcommand)]
        which: AuthCommand,
    },
    /// Drain Telegram replies into the state file (run frequently; write-free re: Actual).
    Poll,
    /// Discover Monzo accounts + Actual accounts and auto-suggest a mapping.
    /// Prints the suggestion; `--write` persists it to the state file.
    Accounts {
        /// Persist the suggested mapping into the state file.
        #[arg(long)]
        write: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Authorise Monzo and print the refresh token to paste into the server state.
    Monzo,
}

#[derive(Clone)]
pub struct Config {
    pub anthropic_api_key: String,
    pub claude_model: String,
    pub monzo_client_id: String,
    pub monzo_client_secret: String,
    pub monzo_redirect_uri: String,
    pub apprise_url: String,
    pub actual_server_url: String,
    pub actual_password: String,
    pub actual_sync_id: String,
    pub state_path: PathBuf,
    pub shim_dir: PathBuf,
    pub monzo_base_url: String,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub autoimport_confidence: String,
    pub lock_path: PathBuf,
    pub telegram_base_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_getter(|k| std::env::var(k).ok())
    }

    /// Build from any getter (used by tests).
    pub fn from_getter(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let req = |k: &str| get(k).ok_or_else(|| anyhow!("missing env var {k}"));
        let req_state = req("ABA_STATE_PATH")?;
        Ok(Self {
            anthropic_api_key: req("ANTHROPIC_API_KEY")?,
            claude_model: get("CLAUDE_MODEL").unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.to_string()),
            monzo_client_id: req("MONZO_CLIENT_ID")?,
            monzo_client_secret: req("MONZO_CLIENT_SECRET")?,
            monzo_redirect_uri: req("MONZO_REDIRECT_URI")?,
            apprise_url: req("APPRISE_URL")?,
            actual_server_url: req("ACTUAL_SERVER_URL")?,
            actual_password: req("ACTUAL_PASSWORD")?,
            actual_sync_id: req("ACTUAL_SYNC_ID")?,
            state_path: PathBuf::from(req_state.clone()), // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
            shim_dir: PathBuf::from(
                get("ABA_SHIM_DIR").unwrap_or_else(|| "/app/actual-shim".to_string()), // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
            ),
            monzo_base_url: get("MONZO_BASE_URL")
                .unwrap_or_else(|| "https://api.monzo.com".to_string()),
            telegram_bot_token: get("TELEGRAM_BOT_TOKEN"),
            telegram_chat_id: get("TELEGRAM_CHAT_ID"),
            autoimport_confidence: get("ABA_AUTOIMPORT_CONFIDENCE")
                .unwrap_or_else(|| "high".to_string()),
            lock_path: get("ABA_LOCK_PATH").map_or_else(
                || {
                    PathBuf::from(req_state.clone()).with_extension("lock") // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
                },
                PathBuf::from,
            ),
            telegram_base_url: get("TELEGRAM_BASE_URL")
                .unwrap_or_else(|| "https://api.telegram.org".to_string()),
        })
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("anthropic_api_key", &"***")
            .field("claude_model", &self.claude_model)
            .field("monzo_client_id", &self.monzo_client_id)
            .field("monzo_client_secret", &"***")
            .field("monzo_redirect_uri", &self.monzo_redirect_uri)
            .field("apprise_url", &"***")
            .field("actual_server_url", &self.actual_server_url)
            .field("actual_password", &"***")
            .field("actual_sync_id", &self.actual_sync_id)
            .field("state_path", &self.state_path)
            .field("shim_dir", &self.shim_dir)
            .field("monzo_base_url", &self.monzo_base_url)
            .field(
                "telegram_bot_token",
                &self.telegram_bot_token.as_ref().map(|_| "***"),
            )
            .field("telegram_chat_id", &self.telegram_chat_id)
            .field("autoimport_confidence", &self.autoimport_confidence)
            .field("lock_path", &self.lock_path)
            .field("telegram_base_url", &self.telegram_base_url)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_reads_from_map() {
        let get = |k: &str| -> Option<String> {
            match k {
                "ANTHROPIC_API_KEY" => Some("ak".into()),
                "MONZO_CLIENT_ID" => Some("cid".into()),
                "MONZO_CLIENT_SECRET" => Some("csec".into()),
                "MONZO_REDIRECT_URI" => Some("http://localhost:8080/callback".into()),
                "APPRISE_URL" => Some("http://apprise/notify".into()),
                "ACTUAL_SERVER_URL" => Some("http://actual:5006".into()),
                "ACTUAL_PASSWORD" => Some("pw".into()),
                "ACTUAL_SYNC_ID" => Some("sync".into()),
                "ABA_STATE_PATH" => Some("/data/aba.state.toml".into()),
                _ => None,
            }
        };
        let c = Config::from_getter(get).unwrap();
        assert_eq!(c.claude_model, "claude-sonnet-4-6");
        assert_eq!(c.actual_server_url, "http://actual:5006");
        assert_eq!(c.state_path.to_string_lossy(), "/data/aba.state.toml");
    }

    #[test]
    fn config_debug_redacts_secrets() {
        let cfg = Config::from_getter(|k| match k {
            "ANTHROPIC_API_KEY" => Some("AK_SECRET".into()),
            "MONZO_CLIENT_ID" => Some("cid".into()),
            "MONZO_CLIENT_SECRET" => Some("CS_SECRET".into()),
            "MONZO_REDIRECT_URI" => Some("http://localhost/cb".into()),
            "APPRISE_URL" => Some("tgram://APP_SECRET/123".into()),
            "ACTUAL_SERVER_URL" => Some("http://actual".into()),
            "ACTUAL_PASSWORD" => Some("PW_SECRET".into()),
            "ACTUAL_SYNC_ID" => Some("sync".into()),
            "ABA_STATE_PATH" => Some("/tmp/s.toml".into()),
            "TELEGRAM_BOT_TOKEN" => Some("BOT_SECRET".into()),
            _ => None,
        })
        .unwrap();
        let s = format!("{cfg:?}");
        for secret in [
            "AK_SECRET",
            "CS_SECRET",
            "PW_SECRET",
            "APP_SECRET",
            "BOT_SECRET",
        ] {
            assert!(!s.contains(secret), "secret {secret} leaked in Debug: {s}");
        }
        assert!(s.contains("cid"), "non-secret client id should show");
    }

    #[test]
    fn missing_required_is_error() {
        let get = |_: &str| -> Option<String> { None };
        assert!(Config::from_getter(get).is_err());
    }

    #[test]
    fn telegram_and_autoimport_defaults_and_overrides() {
        let base = |k: &str| -> Option<String> {
            match k {
                "ANTHROPIC_API_KEY" => Some("ak".into()),
                "MONZO_CLIENT_ID" => Some("cid".into()),
                "MONZO_CLIENT_SECRET" => Some("csec".into()),
                "MONZO_REDIRECT_URI" => Some("http://localhost:8080/callback".into()),
                "APPRISE_URL" => Some("http://apprise/notify".into()),
                "ACTUAL_SERVER_URL" => Some("http://actual:5006".into()),
                "ACTUAL_PASSWORD" => Some("pw".into()),
                "ACTUAL_SYNC_ID" => Some("sync".into()),
                "ABA_STATE_PATH" => Some("/data/aba.state.toml".into()),
                _ => None,
            }
        };
        let c = Config::from_getter(base).unwrap();
        assert_eq!(c.telegram_bot_token, None);
        assert_eq!(c.telegram_chat_id, None);
        assert_eq!(c.autoimport_confidence, "high");
        assert_eq!(c.lock_path.to_string_lossy(), "/data/aba.state.lock");
        assert_eq!(c.telegram_base_url, "https://api.telegram.org");

        let with_tg = |k: &str| -> Option<String> {
            match k {
                "TELEGRAM_BOT_TOKEN" => Some("tok".into()),
                "TELEGRAM_CHAT_ID" => Some("42".into()),
                "ABA_AUTOIMPORT_CONFIDENCE" => Some("medium".into()),
                "ABA_LOCK_PATH" => Some("/custom/aba.lock".into()),
                _ => base(k),
            }
        };
        let c2 = Config::from_getter(with_tg).unwrap();
        assert_eq!(c2.telegram_bot_token.as_deref(), Some("tok"));
        assert_eq!(c2.telegram_chat_id.as_deref(), Some("42"));
        assert_eq!(c2.lock_path.to_string_lossy(), "/custom/aba.lock");
        assert_eq!(c2.autoimport_confidence, "medium");
    }
}
