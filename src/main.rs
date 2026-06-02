mod actual;
mod banks;
mod claude;
mod config;
mod http;
mod lock;
mod matching;
mod notify;
mod poll;
mod run;
mod state;
mod telegram;

use anyhow::Result;
use clap::Parser;

use crate::actual::ActualClient;
use crate::banks::monzo::Monzo;
use crate::claude::{Confidence, HttpClaude};
use crate::config::{AuthCommand, Cli, Command, Config};
use crate::lock::{lock_blocking, lock_try};
use crate::notify::HttpApprise;
use crate::poll::poll;
use crate::state::State;
use crate::telegram::HttpTelegram;

fn build_channel(cfg: &Config) -> Option<HttpTelegram> {
    match (
        cfg.telegram_bot_token.as_ref(),
        cfg.telegram_chat_id.as_ref(),
    ) {
        (Some(tok), Some(chat)) => match chat.parse::<i64>() {
            Ok(chat_id) => Some(
                HttpTelegram::new(tok.clone(), chat_id)
                    .with_base_url(cfg.telegram_base_url.clone()),
            ),
            Err(_) => {
                tracing::warn!("TELEGRAM_CHAT_ID is not a valid integer; Telegram disabled");
                None
            }
        },
        _ => None,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    match cli.command {
        Command::Auth {
            which: AuthCommand::Monzo,
        } => run::auth_monzo(&cfg).await,
        Command::Poll => {
            let Some(channel) = build_channel(&cfg) else {
                tracing::info!("Telegram not configured; poll is a no-op");
                return Ok(());
            };
            // Try-lock: if `run` (or another poll) holds it, skip this tick.
            let Some(_lock) = lock_try(&cfg.lock_path)? else {
                tracing::info!("state locked by another process; skipping this poll");
                return Ok(());
            };
            let mut state = State::load(&cfg.state_path)?;
            match tokio::time::timeout(
                std::time::Duration::from_secs(600),
                poll(&cfg, &mut state, &channel),
            )
            .await
            {
                Ok(res) => { res?; }
                Err(_) => return Err(anyhow::anyhow!("poll timed out after 600s")),
            }
            Ok(())
        }
        Command::Run { dry_run } => {
            let _lock = lock_blocking(&cfg.lock_path)?;
            let mut state = State::load(&cfg.state_path)?;
            let mut bank = Monzo::new(
                cfg.monzo_client_id.clone(),
                cfg.monzo_client_secret.clone(),
                cfg.monzo_base_url.clone(),
                state.monzo_clone(),
            );
            let actual = ActualClient::new(
                cfg.shim_dir.clone(),
                dry_run,
                cfg.actual_server_url.clone(),
                cfg.actual_password.clone(),
                cfg.actual_sync_id.clone(),
            );
            let claude = HttpClaude::new(cfg.anthropic_api_key.clone(), cfg.claude_model.clone());
            let notifier = HttpApprise::new(cfg.apprise_url.clone());
            let channel = build_channel(&cfg);
            let autoimport = Confidence::parse(&cfg.autoimport_confidence);

            match tokio::time::timeout(std::time::Duration::from_secs(600), run::run(
                &cfg,
                dry_run,
                &mut state,
                &mut bank,
                &actual,
                &claude,
                &notifier,
                channel
                    .as_ref()
                    .map(|c| c as &dyn crate::telegram::ConfirmChannel),
                autoimport,
            ))
            .await
            {
                Ok(res) => res,
                Err(_) => Err(anyhow::anyhow!("run timed out after 600s")),
            }
        }
    }
}
