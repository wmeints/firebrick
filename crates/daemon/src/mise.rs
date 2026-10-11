//! Trusts and installs the mise tools of a sandbox's workspace, so a sandbox comes up with the
//! project's tools ready.

use crate::guest_text::strip_control;
use microsandbox::protocol::exec::ExecFailureKind;
use microsandbox::{MicrosandboxError, Sandbox};
use std::collections::BTreeMap;
use std::time::Duration;
use thiserror::Error;

/// Label that stores whether a sandbox installs its workspace's mise tools on start.
pub const ENABLED_LABEL: &str = "firebrick.mise";

/// The mise config files at the workspace root that fbkd trusts, relative to the workspace.
const CONFIG_FILES: [&str; 5] = [
    "mise.toml",
    ".mise.toml",
    "mise/config.toml",
    ".config/mise.toml",
    ".tool-versions",
];

/// How long `mise install` gets to download and install the tools.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long `mise trust` gets per config file.
const TRUST_TIMEOUT: Duration = Duration::from_secs(60);

/// How many trailing lines of mise's stderr end up in the error.
const STDERR_TAIL_LINES: usize = 10;

/// Errors of installing the mise tools.
#[derive(Error, Debug)]
pub enum MiseError {
    /// `mise trust` or `mise install` exited non-zero or timed out.
    #[error("mise install failed in sandbox {sandbox}: {output}")]
    Failed {
        /// Name of the sandbox.
        sandbox: String,
        /// The last lines of mise's stderr, or why it failed when it printed nothing.
        output: String,
    },
    /// The image has no `mise` executable.
    #[error("the image of sandbox {0} has no mise executable")]
    NotInstalled(String),
    /// Running the command in the guest failed.
    #[error("failed to run mise in sandbox {sandbox}")]
    Exec {
        /// Name of the sandbox.
        sandbox: String,
        /// The error of microsandbox.
        #[source]
        source: MicrosandboxError,
    },
}

/// Returns the label value that stores whether mise is enabled.
pub fn label_value(enabled: bool) -> &'static str {
    if enabled { "true" } else { "false" }
}

/// Whether a sandbox with the labels installs its mise tools. Sandboxes without the label were
/// created before it existed and install them.
pub fn is_enabled(labels: &BTreeMap<String, String>) -> bool {
    labels.get(ENABLED_LABEL).map(String::as_str) != Some(label_value(false))
}

/// Returns the guest paths of the mise config files that can exist at the workspace root.
fn config_paths(workspace: &str) -> Vec<String> {
    let workspace = workspace.trim_end_matches('/');
    CONFIG_FILES
        .iter()
        .map(|file| format!("{workspace}/{file}"))
        .collect()
}

/// Returns the last `lines` non-empty lines of `output`.
fn tail(output: &str, lines: usize) -> String {
    let all: Vec<&str> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Trusts each mise config file at the root of `workspace`, then installs the tools, as the
/// image's default user. Does nothing when the workspace has no mise config file.
pub async fn install_tools(sb: &Sandbox, workspace: &str) -> Result<(), MiseError> {
    let configs = existing_configs(sb, workspace).await?;

    if configs.is_empty() {
        return Ok(());
    }

    for config in &configs {
        run(sb, workspace, &["trust", config], TRUST_TIMEOUT).await?;
    }

    run(sb, workspace, &["install", "--yes"], INSTALL_TIMEOUT).await
}

/// Returns the mise config files that exist at the root of the workspace.
async fn existing_configs(sb: &Sandbox, workspace: &str) -> Result<Vec<String>, MiseError> {
    let mut configs = vec![];

    for path in config_paths(workspace) {
        let exists = sb
            .fs()
            .exists(&path)
            .await
            .map_err(|source| exec_error(sb, source))?;

        if exists {
            configs.push(path);
        }
    }

    Ok(configs)
}

/// Runs mise with the arguments in the workspace, failing when it exits non-zero.
async fn run(
    sb: &Sandbox,
    workspace: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<(), MiseError> {
    let result = sb
        .exec_with("mise", |e| {
            e.args(args.iter().copied()).cwd(workspace).timeout(timeout)
        })
        .await;

    match result {
        Ok(output) if output.status().success => Ok(()),
        Ok(output) => Err(failed(
            sb,
            failure_output(output.stderr_bytes(), output.status().code),
        )),
        Err(MicrosandboxError::ExecTimeout(timeout)) => Err(failed(
            sb,
            format!("mise {} timed out after {timeout:?}", args[0]),
        )),
        Err(MicrosandboxError::ExecFailed(failure))
            if failure.kind == ExecFailureKind::NotFound =>
        {
            Err(MiseError::NotInstalled(sb.name().to_string()))
        }
        Err(source) => Err(exec_error(sb, source)),
    }
}

/// Returns the tail of mise's stderr without control characters, or its exit code when it
/// printed nothing.
fn failure_output(stderr: &[u8], code: i32) -> String {
    let stderr = tail(
        &strip_control(&String::from_utf8_lossy(stderr)),
        STDERR_TAIL_LINES,
    );

    if stderr.is_empty() {
        format!("mise exited with code {code}")
    } else {
        stderr
    }
}

fn failed(sb: &Sandbox, output: String) -> MiseError {
    MiseError::Failed {
        sandbox: sb.name().to_string(),
        output,
    }
}

fn exec_error(sb: &Sandbox, source: MicrosandboxError) -> MiseError {
    MiseError::Exec {
        sandbox: sb.name().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_paths_lists_root_config_files() {
        assert_eq!(
            config_paths("/workspaces/demo/"),
            [
                "/workspaces/demo/mise.toml",
                "/workspaces/demo/.mise.toml",
                "/workspaces/demo/mise/config.toml",
                "/workspaces/demo/.config/mise.toml",
                "/workspaces/demo/.tool-versions",
            ]
        );
    }

    #[test]
    fn is_enabled_defaults_to_true() {
        assert!(is_enabled(&BTreeMap::new()));
    }

    #[test]
    fn is_enabled_reads_the_label() {
        for enabled in [true, false] {
            let labels = BTreeMap::from([(ENABLED_LABEL.to_string(), label_value(enabled).into())]);

            assert_eq!(is_enabled(&labels), enabled);
        }
    }

    #[test]
    fn tail_keeps_the_last_non_empty_lines() {
        assert_eq!(tail("a\nb\n\nc\nd\n", 3), "b\nc\nd");
        assert_eq!(tail("only\n", 3), "only");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn failed_error_names_the_sandbox_and_output() {
        let error = MiseError::Failed {
            sandbox: "demo".into(),
            output: "mise ERROR no such version".into(),
        };

        assert_eq!(
            error.to_string(),
            "mise install failed in sandbox demo: mise ERROR no such version"
        );
    }

    #[test]
    fn failure_output_shows_the_exit_code_without_stderr() {
        assert_eq!(failure_output(b"\n", 3), "mise exited with code 3");
    }

    #[test]
    fn failure_output_removes_terminal_escape_sequences() {
        let stderr =
            b"mise ERROR task failed\n\x1b]52;c;Y3VybCBldmlsLmV4YW1wbGUgfCBzaA==\x07boom\n";

        assert_eq!(
            failure_output(stderr, 1),
            "mise ERROR task failed\n]52;c;Y3VybCBldmlsLmV4YW1wbGUgfCBzaA==boom"
        );
    }
}
