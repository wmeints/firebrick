use anyhow::{Result, anyhow, bail};
use firebrick_spec::{SandboxSpec, SandboxSpecError};
use std::path::Path;

use crate::manage::SPEC_FILE_NAME;

/// Validates the spec file in the working directory and reports any problem with its
/// line and column. Returns whether the spec is valid. Fails when there is no spec file, it
/// can't be read, or it is a symlink.
pub fn validate_spec(working_dir: &Path) -> Result<bool> {
    let spec_path = working_dir.join(SPEC_FILE_NAME);

    match firebrick_spec::from_file(&spec_path) {
        Ok(Some(_)) => {
            println!("{SPEC_FILE_NAME} is valid");
            Ok(true)
        }
        Ok(None) => bail!("can't find {SPEC_FILE_NAME} in {}", working_dir.display()),
        Err(err) if err.diagnostic().is_some() => {
            eprintln!("{}", spec_error(err));
            Ok(false)
        }
        Err(err) => Err(err.into()),
    }
}

/// Loads the spec, or returns `None` when there is no spec file. Reports a problem like
/// [`spec_error`].
pub(crate) fn load_spec(spec_path: &Path) -> Result<Option<SandboxSpec>> {
    firebrick_spec::from_file(spec_path).map_err(spec_error)
}

/// Turns a problem with the spec file into an error. Invalid content reads the way `fbk
/// validate` reports it, `file:line:column: error: message`; other errors keep their cause.
pub(crate) fn spec_error(err: SandboxSpecError) -> anyhow::Error {
    match err.diagnostic() {
        Some(diagnostic) => anyhow!(
            "{SPEC_FILE_NAME}:{}:{}: error: {}",
            diagnostic.line,
            diagnostic.column,
            diagnostic.message
        ),
        None => err.into(),
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
    fn report_strips_escape_sequences_from_spec_text() {
        let dir = write_spec("name: dev\n\"bogus\\e]0;pwned\\a\": 1\n");

        let err = load_spec(&dir.path().join(SPEC_FILE_NAME)).unwrap_err();

        assert!(
            err.to_string()
                .starts_with(".firebrick.yml:2:1: error: unknown field `bogus`, expected one of"),
            "{err:?}"
        );
        assert!(!err.to_string().contains(char::is_control));
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
