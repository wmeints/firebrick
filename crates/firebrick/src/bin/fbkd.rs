//! The `fbkd` daemon that manages Firebrick microVM sandboxes for coding agents.

#[cfg(unix)]
use std::fs::DirBuilder;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Picks the microsandbox home, then runs the `fbkd` daemon until it shuts down.
fn main() -> Result<()> {
    use_firebrick_msb_home()?;
    firebrick_daemon::app::main()
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
    // SAFETY: `main` calls this first, before the daemon starts the Tokio runtime or the log
    // writer, while the process has a single thread, so nothing reads or writes the environment
    // concurrently.
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
