use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use firebrick_daemon::secrets::{self, SecretStore};
use firebrick_daemon::{runtime, sandboxes, server, ssh};

/// Picks the data, log and microsandbox directories, then runs the daemon on a Tokio runtime.
fn main() -> Result<()> {
    // Fail instead of falling back to the shared temp dir, where another user could create
    // the directories first and read or plant the secrets, SSH keys and SSH config.
    let secrets_path = firebrick_utils::secrets_path().context(ssh::NO_DATA_DIR)?;
    let log_dir = firebrick_utils::log_dir()
        .context("can't choose a log directory: set HOME or XDG_STATE_HOME to an absolute path")?;
    use_firebrick_msb_home()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the Tokio runtime")?
        .block_on(run(&log_dir, secrets_path))
}

/// Points microsandbox at firebrick's own home when `MSB_HOME` is unset or empty, so `fbkd`
/// never shares the runtime, database and sandboxes of a separately installed `msb`.
///
/// This sets `MSB_HOME` instead of installing a default backend with that home: the `msb`
/// processes microsandbox spawns for the VMs only inherit the environment, and resolve some
/// paths, such as the TLS interception CA, from `MSB_HOME`.
fn use_firebrick_msb_home() -> Result<()> {
    if std::env::var_os("MSB_HOME").is_some_and(|home| !home.is_empty()) {
        return Ok(());
    }
    let Some(home) = firebrick_utils::msb_home() else {
        bail!(
            "can't choose a microsandbox home: set HOME or XDG_STATE_HOME to an absolute path, \
             or set MSB_HOME"
        );
    };
    // Private, because the home holds the `msb` binary fbkd runs and the TLS interception CA.
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&home)
        .with_context(|| format!("failed to create the microsandbox home {}", home.display()))?;
    // SAFETY: `main` calls this before it starts the Tokio runtime or the log writer, while the
    // process has a single thread, so nothing reads or writes the environment concurrently.
    // HUMAN-APPROVED: edition 2024 marks `set_var` unsafe because other threads may read the
    // environment at the same time; the process is still single-threaded here.
    unsafe { std::env::set_var("MSB_HOME", &home) };
    Ok(())
}

/// Sets up logging in `log_dir` and runs the daemon on its unix socket until shutdown, with the
/// secrets stored in `secrets_path`.
async fn run(log_dir: &Path, secrets_path: PathBuf) -> Result<()> {
    let _log_guard = init_logging(log_dir)?;

    // Check the socket before touching the runtime, so a second daemon can't replace the
    // runtime under the one that is already serving.
    let socket_path = firebrick_utils::socket_path();
    if socket_path.exists() {
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
    tracing::info!(
        path = socket_path.to_str().unwrap(),
        "listening on unix socket"
    );
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
