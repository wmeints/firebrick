//! The workspace's `.firebrick.yml`: finds, migrates and loads it, reports its problems the way
//! `fbk validate` does, and names the sandbox of a workspace without one. `fbk init` writes it
//! and `fbk validate` checks it.

pub mod init;
pub mod validate;

use anyhow::{Context, Result, anyhow};
use firebrick_spec::{SandboxSpec, SandboxSpecError};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Name of the spec file `fbk` looks for in the working directory.
pub const SPEC_FILE_NAME: &str = ".firebrick.yml";

/// Returns the legacy name when a sandbox with it exists, and the hashed name otherwise.
pub(crate) fn default_name(working_dir: &Path, legacy_exists: bool) -> String {
    match legacy_name(working_dir) {
        Some(name) if legacy_exists => name,
        _ => hashed_name(working_dir),
    }
}

/// Loads the spec file from the working directory, or returns `None` when there isn't one,
/// after migrating a boolean `mise` in it. Fails when it is a symlink.
pub(crate) fn read_spec_file(working_dir: &Path) -> Result<Option<SandboxSpec>> {
    let path = working_dir.join(SPEC_FILE_NAME);
    migrate_spec(&path)?;

    Ok(firebrick_spec::from_file(&path)?)
}

/// Names the sandbox `firebrick-` followed by the first 6 hex digits of the SHA-256 hash of
/// the full path.
fn hashed_name(working_dir: &Path) -> String {
    let digest = Sha256::digest(working_dir.as_os_str().as_encoded_bytes());
    let prefix: String = digest[..3]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    format!("firebrick-{prefix}")
}

/// Encodes the full path as ASCII alphanumerics and underscores, the way sandboxes were named
/// before names were hashed. Returns `None` when the path has no alphanumerics.
pub(crate) fn legacy_name(working_dir: &Path) -> Option<String> {
    let mut name = String::new();

    for c in working_dir.to_string_lossy().chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('_') {
            name.push('_');
        }
    }

    let name = name.trim_end_matches('_');

    (!name.is_empty()).then(|| name.to_string())
}

/// Loads the spec, or returns `None` when there is no spec file. Reports a problem like
/// [`spec_error`].
pub(crate) fn load_spec(spec_path: &Path) -> Result<Option<SandboxSpec>> {
    migrate_spec(spec_path)?;
    firebrick_spec::from_file(spec_path).map_err(spec_error)
}

/// Rewrites a `mise: true` or `mise: false` in the spec file to the `mise.enabled` it became,
/// telling the user on stderr. Does nothing when there is no spec file or nothing to migrate.
/// Refuses a symlink like [`firebrick_spec::read_text`].
pub(crate) fn migrate_spec(spec_path: &Path) -> Result<()> {
    let text = firebrick_spec::read_text(spec_path)?;
    let Some((migrated, enabled)) = text.as_deref().and_then(firebrick_spec::migrate_mise) else {
        return Ok(());
    };

    firebrick_spec::write_text(spec_path, &migrated).with_context(|| {
        format!(
            "can't migrate mise in {}; replace `mise: {enabled}` with `mise:` and an indented \
             `enabled: {enabled}` by hand",
            spec_path.display()
        )
    })?;
    eprintln!("Migrated mise: {enabled} in {SPEC_FILE_NAME} to mise.enabled: {enabled}");

    Ok(())
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
    use std::fs;
    use tempfile::TempDir;

    /// Returns a working directory with a spec file with the content.
    pub(super) fn write_spec(content: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), content).unwrap();
        dir
    }

    #[test]
    fn hashed_name_uses_first_six_hex_digits_of_sha256() {
        let name = hashed_name(Path::new("/home/user/my-project.v2"));

        assert_eq!(name, "firebrick-d9f287");
    }

    #[test]
    fn hashed_name_differs_per_path() {
        assert_ne!(
            hashed_name(Path::new("/home/user/a/project")),
            hashed_name(Path::new("/home/user/b/project"))
        );
    }

    #[test]
    fn legacy_name_encodes_full_path() {
        let name = legacy_name(Path::new("/home/user/my-project.v2"));

        assert_eq!(name.as_deref(), Some("home_user_my_project_v2"));
    }

    #[test]
    fn legacy_name_collapses_and_trims_separators() {
        let name = legacy_name(Path::new("//home//user/--project--/"));

        assert_eq!(name.as_deref(), Some("home_user_project"));
    }

    #[test]
    fn default_name_keeps_existing_legacy_name() {
        let name = default_name(Path::new("/home/user/my-project.v2"), true);

        assert_eq!(name, "home_user_my_project_v2");
    }

    #[test]
    fn default_name_hashes_path_without_legacy_sandbox() {
        let name = default_name(Path::new("/home/user/my-project.v2"), false);

        assert_eq!(name, "firebrick-d9f287");
    }

    #[test]
    fn root_path_has_no_legacy_name() {
        assert_eq!(legacy_name(Path::new("/")), None);
    }

    #[test]
    fn loading_migrates_a_boolean_mise() {
        let dir = write_spec("name: dev\nmise: false\n");

        let spec = load_spec(&dir.path().join(SPEC_FILE_NAME))
            .unwrap()
            .unwrap();

        assert_eq!(spec.mise.and_then(|mise| mise.enabled), Some(false));
    }

    #[cfg(unix)]
    #[test]
    fn failed_migration_says_how_to_migrate_by_hand() {
        use std::os::unix::fs::PermissionsExt;

        let dir = write_spec("name: dev\nmise: false\n");
        let read_only = fs::Permissions::from_mode(0o555);
        fs::set_permissions(dir.path(), read_only).unwrap();

        let result = load_spec(&dir.path().join(SPEC_FILE_NAME));
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.unwrap_err();
        assert!(
            err.to_string().starts_with("can't migrate mise in")
                && err.to_string().contains("by hand"),
            "{err}"
        );
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
}
