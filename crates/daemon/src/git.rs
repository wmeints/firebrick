//! Writes the host's git identity into the global git config of a sandbox's default user, so
//! commits in the sandbox carry the same author as commits on the host.

use microsandbox::{MicrosandboxError, Sandbox};
use std::time::Duration;
use thiserror::Error;

/// How long one `git config` command gets in the sandbox.
const CONFIG_TIMEOUT: Duration = Duration::from_secs(5);

/// The git identity to give a sandbox. A key without a value keeps its value in the sandbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitIdentity<'a> {
    /// The value for `user.name`.
    pub name: Option<&'a str>,
    /// The value for `user.email`.
    pub email: Option<&'a str>,
}

impl<'a> GitIdentity<'a> {
    /// Returns the git config keys with a value, with that value.
    fn entries(self) -> impl Iterator<Item = (&'static str, &'a str)> {
        [("user.name", self.name), ("user.email", self.email)]
            .into_iter()
            .filter_map(|(key, value)| Some((key, value?)))
    }
}

/// Errors of setting one git config key in a sandbox.
#[derive(Error, Debug)]
enum SetError {
    /// Running `git` in the guest failed, for example because the image has no git.
    #[error(transparent)]
    Exec(#[from] MicrosandboxError),
    /// `git config` exited non-zero.
    #[error("git config exited with code {code}: {stderr}")]
    Exited {
        /// The exit code.
        code: i32,
        /// What git wrote to stderr.
        stderr: String,
    },
}

/// Sets each key of the identity that has a value in the global git config of the running
/// sandbox's default user. Runs `git` without a shell, so the values are stored literally. A
/// failure only logs a warning, because a sandbox without git or a git config is still usable.
pub async fn apply(sb: &Sandbox, identity: GitIdentity<'_>) {
    for (key, value) in identity.entries() {
        if let Err(err) = set(sb, key, value).await {
            tracing::warn!("failed to set git {key} in sandbox {}: {err}", sb.name());
        }
    }
}

/// Sets the git config key to the value with `git config --global` as the default user. It runs
/// in `/` rather than the sandbox's workdir, because git also reads the config of the
/// workspace's repository there, which fails while the host is rewriting it.
async fn set(sb: &Sandbox, key: &str, value: &str) -> Result<(), SetError> {
    let output = sb
        .exec_with("git", |exec| {
            exec.args(["config", "--global", key, value])
                .cwd("/")
                .timeout(CONFIG_TIMEOUT)
        })
        .await?;
    let status = output.status();

    if status.success {
        return Ok(());
    }

    Err(SetError::Exited {
        code: status.code,
        stderr: String::from_utf8_lossy(output.stderr_bytes())
            .trim()
            .to_string(),
    })
}
