mod actual;
mod banks;
mod claude;
mod config;
mod lock;
mod matching;
mod notify;
mod run;
mod state;
mod telegram;

use anyhow::Result;
use clap::Parser;

use crate::actual::ActualClient;
use crate::banks::monzo::Monzo;
use crate::claude::HttpClaude;
use crate::config::{AuthCommand, Cli, Command, Config};
use crate::notify::HttpApprise;
use crate::state::State;

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
        Command::Run { dry_run } => {
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

            run::run(
                &cfg, dry_run, &mut state, &mut bank, &actual, &claude, &notifier,
            )
            .await
        }
    }
}
