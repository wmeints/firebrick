use anyhow::{Result, anyhow, bail};
use firebrick_spec::{SandboxSpec, SandboxSpecError};
use std::path::Path;

use crate::manage::SPEC_FILE_NAME;

/// Validates the spec file in the working directory and reports any problem with its
/// line and column. Returns whether the spec is valid. Fails when there is no spec file or it
/// is a symlink.
pub fn validate_spec(working_dir: &Path) -> Result<bool> {
    let spec_path = working_dir.join(SPEC_FILE_NAME);

    let Some(text) = read_spec_text(&spec_path)? else {
        bail!("can't find {SPEC_FILE_NAME} in {}", working_dir.display());
    };

    match parse_spec(&text) {
        Ok(_) => {
            println!("{SPEC_FILE_NAME} is valid");
            Ok(true)
        }
        Err(report) => {
            eprintln!("{report}");
            Ok(false)
        }
    }
}

/// Loads the spec, or returns `None` when there is no spec file. Formats a problem the way
/// `fbk validate` reports it: `file:line:column: error: message`.
pub(crate) fn load_spec(spec_path: &Path) -> Result<Option<SandboxSpec>> {
    let Some(text) = read_spec_text(spec_path)? else {
        return Ok(None);
    };

    parse_spec(&text)
        .map(Some)
        .map_err(|report| anyhow!(report))
}

/// Reads the text of the spec file, or returns `None` when there is none. Fails when it is a
/// symlink.
pub(crate) fn read_spec_text(spec_path: &Path) -> Result<Option<String>> {
    firebrick_spec::read_text(spec_path).map_err(|err| anyhow!(report(&err)))
}

/// Parses the text of the spec file, formatting a problem the way `fbk validate` reports it.
fn parse_spec(text: &str) -> Result<SandboxSpec, String> {
    firebrick_spec::from_str(text).map_err(|err| report(&err))
}

/// Formats a problem with the spec file, with its position when it has one. A symlinked spec
/// file is reported as is, since the error already names the file.
pub(crate) fn report(err: &SandboxSpecError) -> String {
    match (err.diagnostic(), err) {
        (Some(diagnostic), _) => format!(
            "{SPEC_FILE_NAME}:{}:{}: error: {}",
            diagnostic.line, diagnostic.column, diagnostic.message
        ),
        (None, SandboxSpecError::Symlink(_)) => err.to_string(),
        (None, _) => format!("{SPEC_FILE_NAME}: error: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symlinked_spec;
    use std::fs;
    use tempfile::TempDir;

    fn write_spec(content: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), content).unwrap();
        dir
    }

    #[test]
    fn valid_spec_passes() {
        let dir = write_spec("name: dev\nimage: ubuntu:24.04\n");

        assert!(validate_spec(dir.path()).unwrap());
    }

    #[test]
    fn invalid_spec_fails() {
        let dir = write_spec("image: ubuntu:24.04\n");

        assert!(!validate_spec(dir.path()).unwrap());
    }

    #[test]
    fn missing_spec_returns_error() {
        let dir = TempDir::new().unwrap();

        assert!(validate_spec(dir.path()).is_err());
    }

    #[test]
    fn report_includes_line_and_column() {
        let dir = write_spec("name: dev\nresources:\n  cpu: 2\n  memroy: 4Gi\n");

        let err = load_spec(&dir.path().join(SPEC_FILE_NAME)).unwrap_err();

        assert_eq!(
            err.to_string(),
            ".firebrick.yml:4:3: error: resources: unknown field `memroy`, expected `cpu` or `memory`"
        );
    }

    #[test]
    fn symlinked_spec_returns_error() {
        for content in [Some("name: dev\n"), None] {
            let (dir, _target) = symlinked_spec::workspace(content);

            let err = validate_spec(dir.path()).unwrap_err();

            assert_eq!(err.to_string(), symlinked_spec::SYMLINK_ERROR);
        }
    }
}
