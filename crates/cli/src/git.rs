//! Reads the host's git identity for a workspace, so fbkd can give the sandbox the same commit
//! author as the host.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

use crate::api::GitIdentity;

/// The exit code of `git config --get` for a key that isn't set.
const UNSET_KEY_CODE: i32 = 1;

/// The variables that point git at a repository. Git sets them for its hooks, so `fbk` started
/// from a hook would otherwise read that repository's identity instead of the workspace's.
pub const REPOSITORY_VARS: [&str; 4] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
];

/// Returns the host's `user.name` and `user.email` as git resolves them in the workspace, like
/// [`read_identity`] with the `git` on `PATH`.
pub async fn host_identity(workspace: &Path) -> Option<GitIdentity> {
    read_identity(OsStr::new("git"), workspace).await
}

/// Returns `user.name` and `user.email` as the `git` executable resolves them in the workspace,
/// so `includeIf` and repository-local identities apply. A key that is unset, or that can't be
/// read because git is missing, is left out. So is one git fails to read or whose value doesn't
/// fit on one line, with a warning on stderr. Returns `None` when both keys are left out.
pub async fn read_identity(git: &OsStr, workspace: &Path) -> Option<GitIdentity> {
    let identity = GitIdentity {
        name: config_value(git, workspace, "user.name").await,
        email: config_value(git, workspace, "user.email").await,
    };

    (identity.name.is_some() || identity.email.is_some()).then_some(identity)
}

/// Returns the value of the git config key in the workspace, or `None` when git can't be run or
/// the value can't be used.
async fn config_value(git: &OsStr, workspace: &Path, key: &str) -> Option<String> {
    let mut command = Command::new(git);
    for var in REPOSITORY_VARS {
        command.env_remove(var);
    }
    let output = command
        .arg("-C")
        .arg(workspace)
        .args(["config", "--get", key])
        .stdin(Stdio::null())
        .output()
        .await;

    let output = match output {
        Ok(output) => output,
        Err(err) => {
            tracing::debug!("couldn't run git to read {key}: {err}");
            return None;
        }
    };

    parse_value(output.status.code(), &output.stdout, &output.stderr).unwrap_or_else(|reason| {
        eprintln!("not copying git {key} into the sandbox: {reason}");
        None
    })
}

/// Turns the exit code and output of `git config --get` into the value, without the newline git
/// ends it with, or `None` for an unset key. Fails with the reason when git failed otherwise, for
/// example on a broken config file, or when the value isn't UTF-8 or has a newline or NUL, which
/// git can't store on one line.
fn parse_value(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Result<Option<String>, String> {
    match code {
        Some(0) => {}
        Some(UNSET_KEY_CODE) => return Ok(None),
        _ => {
            let stderr = String::from_utf8_lossy(stderr);
            return Err(format!("git config failed: {}", stderr.trim()));
        }
    }

    let text = std::str::from_utf8(stdout).map_err(|_| "the value isn't valid UTF-8")?;
    let value = text.strip_suffix('\n').unwrap_or(text);

    if value.contains(['\n', '\0']) {
        return Err("the value has a newline or NUL".to_string());
    }

    Ok(Some(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_loses_the_trailing_newline() {
        assert_eq!(
            parse_value(Some(0), b"Ada Lovelace\n", b""),
            Ok(Some("Ada Lovelace".to_string()))
        );
    }

    #[test]
    fn value_keeps_quotes_and_shell_metacharacters() {
        assert_eq!(
            parse_value(Some(0), b"O'Brien; $(id)\n", b""),
            Ok(Some("O'Brien; $(id)".to_string()))
        );
    }

    #[test]
    fn unset_key_has_no_value() {
        assert_eq!(parse_value(Some(1), b"", b""), Ok(None));
    }

    #[test]
    fn other_git_failure_is_reported_with_its_error() {
        let stderr = b"fatal: bad config line 3 in file /home/ada/.gitconfig\n";

        assert_eq!(
            parse_value(Some(128), b"", stderr),
            Err("git config failed: fatal: bad config line 3 in file /home/ada/.gitconfig".into())
        );
    }

    #[test]
    fn git_killed_by_a_signal_is_reported() {
        assert!(parse_value(None, b"", b"").is_err());
    }

    #[test]
    fn value_with_a_newline_is_refused() {
        assert_eq!(
            parse_value(Some(0), b"Ada\nLovelace\n", b""),
            Err("the value has a newline or NUL".to_string())
        );
    }

    #[test]
    fn value_with_a_nul_is_refused() {
        assert_eq!(
            parse_value(Some(0), b"ada\0@example.com\n", b""),
            Err("the value has a newline or NUL".to_string())
        );
    }

    #[test]
    fn value_that_is_not_utf8_is_refused() {
        assert_eq!(
            parse_value(Some(0), b"\xff\n", b""),
            Err("the value isn't valid UTF-8".to_string())
        );
    }
}
