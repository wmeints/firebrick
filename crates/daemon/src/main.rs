#[cfg(unix)]
use std::fs::DirBuilder;
use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use firebrick_daemon::secrets::{self, SecretStore};
use firebrick_daemon::{runtime, sandboxes, server, ssh};

/// The firebrick daemon. `fbk` starts it on demand, so it takes no arguments besides `--help`
/// and `--version`.
#[derive(Debug, Parser)]
#[command(name = "fbkd", version)]
struct Cli {}

/// Picks the data, log and microsandbox directories, then runs the daemon on a Tokio runtime.
fn main() -> Result<()> {
    Cli::parse();
    // Fail instead of falling back to the shared temp dir, where another user could create
    // the directories first and read or plant the secrets, SSH keys and SSH config.
    let secrets_path = firebrick_utils::secrets_path().context(firebrick_utils::NO_DATA_DIR)?;
    let log_dir = firebrick_utils::log_dir().context(firebrick_utils::NO_LOG_DIR)?;
    use_firebrick_msb_home()?;
    let _lock = lock_daemon()?;
    // Since edition 2024 the runtime, a temporary of the tail expression, is dropped before the
    // lock, so its tasks, which may hold port forwards and listeners, are gone when the lock is
    // released.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the Tokio runtime")?
        .block_on(run(&log_dir, secrets_path))
}

/// Takes the daemon's lock, which this process keeps until it exits, so `fbk` can wait for a
/// daemon it asked to shut down to release its host ports and listeners. Fails when another
/// daemon holds it.
fn lock_daemon() -> Result<File> {
    let path = firebrick_utils::daemon_lock_path().context(firebrick_utils::NO_DATA_DIR)?;
    let lock = firebrick_utils::try_lock_daemon(&path)
        .with_context(|| format!("failed to lock {}", path.display()))?;

    lock.with_context(|| format!("another firebrick daemon holds {}", path.display()))
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
    create_private_home(&home)
        .with_context(|| format!("failed to create the microsandbox home {}", home.display()))?;
    // SAFETY: `main` calls this before it starts the Tokio runtime or the log writer, while the
    // process has a single thread, so nothing reads or writes the environment concurrently.
    // HUMAN-APPROVED: edition 2024 marks `set_var` unsafe because other threads may read the
    // environment at the same time; the process is still single-threaded here.
    unsafe { std::env::set_var("MSB_HOME", &home) };
    Ok(())
}

/// Creates the microsandbox home and the directories above it with mode `0700`.
#[cfg(unix)]
fn create_private_home(home: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(home)
}

/// Creates the microsandbox home with an access list for the user, the system and the
/// administrators only, which everything in it inherits.
#[cfg(windows)]
fn create_private_home(home: &Path) -> std::io::Result<()> {
    firebrick_utils::make_private_dir(home)
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_prints_package_version() {
        let error = Cli::try_parse_from(["fbkd", "--version"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            error.to_string(),
            format!("fbkd {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn rejects_unknown_arguments() {
        let error = Cli::try_parse_from(["fbkd", "serve"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
