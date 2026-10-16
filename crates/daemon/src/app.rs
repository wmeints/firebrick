//! The `fbkd` daemon: picks its directories and serves the sandbox management API.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::secrets::{self, SecretStore};
use crate::{runtime, sandboxes, server, ssh};

/// Picks the data and log directories, then runs the daemon on a Tokio runtime.
///
/// microsandbox keeps its runtime, database and sandboxes in `MSB_HOME`, so the `fbkd` binary
/// sets it to firebrick's own home before it calls this.
pub fn main() -> Result<()> {
    // Fail instead of falling back to the shared temp dir, where another user could create
    // the directories first and read or plant the secrets, SSH keys and SSH config.
    let secrets_path = firebrick_utils::secrets_path().context(firebrick_utils::NO_DATA_DIR)?;
    let log_dir = firebrick_utils::log_dir().context(firebrick_utils::NO_LOG_DIR)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the Tokio runtime")?
        .block_on(run(&log_dir, secrets_path))
}

/// Sets up logging in `log_dir` and runs the daemon on its endpoint until shutdown, with the
/// secrets stored in `secrets_path`.
async fn run(log_dir: &Path, secrets_path: PathBuf) -> Result<()> {
    let _log_guard = init_logging(log_dir)?;

    // Check the socket before touching the runtime, so a second daemon can't replace the
    // runtime under the one that is already serving.
    let socket_path = firebrick_utils::socket_path().context("can't name the daemon's endpoint")?;
    if firebrick_utils::ipc::exists(&socket_path).await {
        bail!(server::ServerError::SocketAlreadyInUse());
    }

    let msb_config =
        microsandbox::config::config().context("failed to load the microsandbox configuration")?;
    runtime::ensure(&msb_config).await?;
    secrets::protect_database(&msb_config.home())
        .context("failed to protect the microsandbox database")?;
    ssh::ensure_keys()?;
    sandboxes::sync_ssh_config().await;

    tracing::info!(path = %log_dir.display(), "writing logs");
    tracing::info!(path = %msb_config.home().display(), "using microsandbox home");
    tracing::info!(path = %socket_path.display(), "listening");
    Ok(server::run(&socket_path, SecretStore::new(secrets_path)).await?)
}

/// Logs to stdout and to a daily log file in `log_dir`. Logs are written until the returned guard
/// is dropped.
fn init_logging(log_dir: &Path) -> Result<WorkerGuard> {
    std::fs::create_dir_all(log_dir)?;
    let (log_writer, log_guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(log_dir, "fbkd.log"));

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(fmt::layer().with_ansi(false).with_writer(log_writer))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    Ok(log_guard)
}
