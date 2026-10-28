//! The errors of reading a spec, and where in the file a problem is.

use std::path::PathBuf;

use thiserror::Error;

/// Errors that can occur while loading a sandbox spec.
#[derive(Error, Debug)]
pub enum SandboxSpecError {
    #[error("can't read {}", .path.display())]
    CantRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("can't parse input yaml: {0}")]
    InvalidSpec(#[from] serde_yaml::Error),
    #[error("can't serialize the spec")]
    CantSerialize(#[source] serde_yaml::Error),
    #[error("can't write {}", .path.display())]
    CantWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The spec file is a symlink, which could make firebrick read or write a file outside the
    /// workspace. Holds the file name.
    #[error("{0} is a symlink; firebrick only uses a regular file in the workspace")]
    Symlink(String),
    /// The spec file is a directory, a FIFO or another file that isn't a regular file, which
    /// could make firebrick fail or hang. Holds the file name.
    #[error("{0} isn't a regular file; firebrick only uses a regular file in the workspace")]
    NotRegularFile(String),
}

/// Errors that can occur while parsing a memory or disk size.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum SizeError {
    #[error(
        "invalid size `{0}`, expected a positive number with a unit, such as `512 MiB` or `4Gi`"
    )]
    Invalid(String),
    #[error("size `{0}` is too large")]
    TooLarge(String),
}

/// A problem in a spec file, pinned to the 1-based line and column it occurs at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecDiagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl SandboxSpecError {
    /// Returns the position and description of the problem when the spec content is invalid.
    pub fn diagnostic(&self) -> Option<SpecDiagnostic> {
        let SandboxSpecError::InvalidSpec(err) = self else {
            return None;
        };

        // Errors without a location concern the document as a whole.
        let (line, column) = err
            .location()
            .map_or((1, 1), |location| (location.line(), location.column()));

        // The yaml error message ends with the position, which we report separately.
        let message = err.to_string();
        let suffix = format!(" at line {line} column {column}");
        // It quotes spec text, such as an unknown key, so it loses its escape sequences.
        let message =
            firebrick_utils::strip_control(message.strip_suffix(&suffix).unwrap_or(&message));

        Some(SpecDiagnostic {
            line,
            column,
            message,
        })
    }
}
