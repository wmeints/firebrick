//! Reads the host's git identity for a workspace, so fbkd can give the sandbox the same commit
//! author as the host.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

use crate::api::GitIdentity;

/// Returns the host's `user.name` and `user.email` as git resolves them in the workspace, so
/// `includeIf` and repository-local identities apply. A key that is unset, can't be read because
/// git is missing, or has a value that doesn't fit on one line is left out. Returns `None` when
/// both keys are.
pub async fn host_identity(workspace: &Path) -> Option<GitIdentity> {
    identity_with(OsStr::new("git"), workspace).await
}

/// Reads the identity like [`host_identity`] with the `git` executable.
async fn identity_with(git: &OsStr, workspace: &Path) -> Option<GitIdentity> {
    let identity = GitIdentity {
        name: config_value(git, workspace, "user.name").await,
        email: config_value(git, workspace, "user.email").await,
    };

    (identity.name.is_some() || identity.email.is_some()).then_some(identity)
}

/// Returns the value of the git config key in the workspace, or `None` when git can't be run or
/// the value can't be used.
async fn config_value(git: &OsStr, workspace: &Path, key: &str) -> Option<String> {
    let output = Command::new(git)
        .arg("-C")
        .arg(workspace)
        .args(["config", "--get", key])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;

    match output {
        Ok(output) => parse_value(key, output.status.success(), &output.stdout),
        Err(err) => {
            tracing::debug!("couldn't run git to read {key}: {err}");
            None
        }
    }
}

/// Turns the output of `git config --get` into the value, without the newline git ends it with.
/// Returns `None` when git failed, which it does for an unset key, or when the value isn't UTF-8
/// or has a newline or NUL, which git can't store on one line.
fn parse_value(key: &str, success: bool, stdout: &[u8]) -> Option<String> {
    if !success {
        tracing::debug!("{key} isn't set in the host's git config");
        return None;
    }

    let Ok(text) = std::str::from_utf8(stdout) else {
        eprintln!("not copying git {key} into the sandbox: the value isn't valid UTF-8");
        return None;
    };
    let value = text.strip_suffix('\n').unwrap_or(text);

    if value.contains(['\n', '\0']) {
        eprintln!("not copying git {key} into the sandbox: the value has a newline or NUL");
        return None;
    }

    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_loses_the_trailing_newline() {
        assert_eq!(
            parse_value("user.name", true, b"Ada Lovelace\n"),
            Some("Ada Lovelace".to_string())
        );
    }

    #[test]
    fn value_keeps_quotes_and_shell_metacharacters() {
        assert_eq!(
            parse_value("user.name", true, b"O'Brien; $(id)\n"),
            Some("O'Brien; $(id)".to_string())
        );
    }

    #[test]
    fn unset_key_has_no_value() {
        assert_eq!(parse_value("user.name", false, b""), None);
    }

    #[test]
    fn value_with_a_newline_is_left_out() {
        assert_eq!(parse_value("user.name", true, b"Ada\nLovelace\n"), None);
    }

    #[test]
    fn value_with_a_nul_is_left_out() {
        assert_eq!(
            parse_value("user.email", true, b"ada\0@example.com\n"),
            None
        );
    }

    #[test]
    fn value_that_is_not_utf8_is_left_out() {
        assert_eq!(parse_value("user.name", true, b"\xff\n"), None);
    }

    #[tokio::test]
    async fn missing_git_gives_no_identity() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join("no-such-git");

        assert_eq!(identity_with(git.as_os_str(), dir.path()).await, None);
    }
}
