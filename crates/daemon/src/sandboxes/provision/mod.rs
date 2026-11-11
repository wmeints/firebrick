//! Applies the parts of a sandbox's spec to the sandbox: its environment variables, the host's
//! git identity, the Java trust store, the workspace's mise tools, the image pull and the
//! network policy.

pub mod env;
pub(crate) mod git;
pub(crate) mod java;
pub mod mise;
pub(crate) mod network;
pub(crate) mod pull;

use microsandbox::sandbox::ExecOutput;
use thiserror::Error;

/// A command in the guest that exited non-zero.
#[derive(Error, Debug, PartialEq, Eq)]
#[error("exited with code {code}: {stderr}")]
pub struct ExitError {
    /// The exit code.
    pub code: i32,
    /// What the command wrote to stderr, without surrounding whitespace.
    pub stderr: String,
}

impl ExitError {
    /// Returns the error of a command that exited with the code and wrote the stderr.
    fn new(code: i32, stderr: &[u8]) -> Self {
        Self {
            code,
            stderr: String::from_utf8_lossy(stderr).trim().to_string(),
        }
    }
}

/// Fails with an [`ExitError`] when the command of the output exited non-zero.
pub(crate) fn check_exit(output: &ExecOutput) -> Result<(), ExitError> {
    let status = output.status();

    if status.success {
        return Ok(());
    }

    Err(ExitError::new(status.code, output.stderr_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_error_names_the_code_and_the_trimmed_stderr() {
        let err = ExitError::new(1, b"openssl is not installed\n");

        assert_eq!(
            err.to_string(),
            "exited with code 1: openssl is not installed"
        );
    }
}
