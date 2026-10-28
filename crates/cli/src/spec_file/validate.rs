//! `fbk validate`: checks the spec file and reports its problems.

use anyhow::{Result, bail};
use std::path::Path;

use super::{SPEC_FILE_NAME, migrate_spec, spec_error};

/// Validates the spec file in the working directory and reports any problem with its
/// line and column. Returns whether the spec is valid. Fails when there is no spec file, it
/// can't be read, or it is a symlink.
pub fn validate_spec(working_dir: &Path) -> Result<bool> {
    let spec_path = working_dir.join(SPEC_FILE_NAME);
    migrate_spec(&spec_path)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec_file::tests::write_spec;
    use crate::test_support::symlinked_spec;
    use std::fs;
    use tempfile::TempDir;

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
    fn validating_migrates_a_boolean_mise() {
        let dir = write_spec("name: dev\nmise: true\n");

        assert!(validate_spec(dir.path()).unwrap());
        assert_eq!(
            fs::read_to_string(dir.path().join(SPEC_FILE_NAME)).unwrap(),
            "name: dev\nmise:\n  enabled: true\n"
        );
    }

    #[test]
    fn missing_spec_returns_error() {
        let dir = TempDir::new().unwrap();

        assert!(validate_spec(dir.path()).is_err());
    }

    #[test]
    fn symlinked_spec_returns_error() {
        for content in [Some("name: dev\n"), None] {
            assert_refuses_symlinked_spec(content);
        }
    }

    /// Asserts that validating refuses a `.firebrick.yml` that links to a file with `content`,
    /// or a missing one.
    fn assert_refuses_symlinked_spec(content: Option<&str>) {
        let Some((dir, _target)) = symlinked_spec::workspace(content) else {
            return;
        };

        let err = validate_spec(dir.path()).unwrap_err();

        assert_eq!(err.to_string(), symlinked_spec::SYMLINK_ERROR);
    }
}
