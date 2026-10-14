use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::manage::SPEC_FILE_NAME;

/// Writes the default spec, named after the working directory, to `.firebrick.yml` in the
/// working directory and returns its path. Refuses to replace an existing file unless `force`
/// is set, and refuses a symlink either way.
pub fn init_spec(working_dir: &Path, force: bool) -> Result<PathBuf> {
    let spec_path = working_dir.join(SPEC_FILE_NAME);
    let spec = firebrick_spec::default_spec(firebrick_utils::sanitize_label(working_dir));
    let content = serde_yaml::to_string(&spec).context("can't serialize the default spec")?;

    if !force && firebrick_spec::exists(&spec_path)? {
        bail!(
            "{SPEC_FILE_NAME} already exists in {}; use fbk init --force to overwrite it",
            working_dir.display()
        );
    }

    firebrick_spec::write_text(&spec_path, &content)?;

    Ok(spec_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symlinked_spec;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn project_dir(name: &str) -> (TempDir, PathBuf) {
        let root = TempDir::new().unwrap();
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        (root, dir)
    }

    #[test]
    fn creates_default_spec() {
        let (_root, dir) = project_dir("project");

        let spec_path = init_spec(&dir, false).unwrap();
        let spec = firebrick_spec::from_file(&spec_path).unwrap().unwrap();
        let resources = spec.resources.unwrap();

        assert_eq!(spec_path, dir.join(SPEC_FILE_NAME));
        assert_eq!(spec.name, "project");
        assert_eq!(spec.image, None);
        assert_eq!(resources.cpu, 2);
        assert_eq!(resources.memory, "4 GiB");
        assert_eq!(spec.init, Some(true));
        assert_eq!(spec.mise, Some(true));
        assert_eq!(spec.volumes, firebrick_spec::VolumesSpec::default());
        assert!(spec.mounts.is_none());
    }

    #[test]
    fn default_spec_leaves_out_mounts() {
        let (_root, dir) = project_dir("project");

        let spec_path = init_spec(&dir, false).unwrap();

        let content = fs::read_to_string(spec_path).unwrap();
        assert!(!content.contains("mounts"), "{content}");
    }

    #[test]
    fn name_is_sanitized_leaf_of_working_dir() {
        for (leaf, name) in [("My.App", "my-app"), ("my project_", "my-project")] {
            let (_root, dir) = project_dir(leaf);

            let spec = firebrick_spec::from_file(&init_spec(&dir, false).unwrap())
                .unwrap()
                .unwrap();

            assert_eq!(spec.name, name);
        }
    }

    #[test]
    fn name_falls_back_to_sandbox() {
        let (_root, dir) = project_dir("__");

        let spec = firebrick_spec::from_file(&init_spec(&dir, false).unwrap())
            .unwrap()
            .unwrap();

        assert_eq!(spec.name, "sandbox");
    }

    #[test]
    fn refuses_to_overwrite_existing_spec() {
        let (_root, dir) = project_dir("project");
        let spec_path = dir.join(SPEC_FILE_NAME);
        fs::write(&spec_path, "name: mine\n").unwrap();

        let err = init_spec(&dir, false).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                ".firebrick.yml already exists in {}; use fbk init --force to overwrite it",
                dir.display()
            )
        );
        assert_eq!(fs::read_to_string(&spec_path).unwrap(), "name: mine\n");
    }

    #[test]
    fn force_overwrites_existing_spec() {
        let (_root, dir) = project_dir("project");
        let spec_path = dir.join(SPEC_FILE_NAME);
        fs::write(
            &spec_path,
            "name: mine\nimage: ubuntu:24.04\n# a long trailing comment\n",
        )
        .unwrap();

        init_spec(&dir, true).unwrap();
        let spec = firebrick_spec::from_file(&spec_path).unwrap().unwrap();

        assert_eq!(spec.name, "project");
        assert_eq!(spec.image, None);
    }

    #[test]
    fn refuses_a_symlinked_spec_and_leaves_its_target_alone() {
        for (content, force) in [
            (Some("notes\n"), false),
            (Some("notes\n"), true),
            (None, false),
            (None, true),
        ] {
            let (dir, target) = symlinked_spec::workspace(content);

            let err = init_spec(dir.path(), force).unwrap_err();

            assert_eq!(err.to_string(), symlinked_spec::SYMLINK_ERROR);
            assert_eq!(symlinked_spec::target_content(&target).as_deref(), content);
        }
    }

    #[test]
    fn write_failure_names_the_path() {
        let (_root, dir) = project_dir("project");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

        let result = init_spec(&dir, false);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains(&dir.join(SPEC_FILE_NAME).display().to_string()),
            "{err}"
        );
        assert!(err.root_cause().downcast_ref::<std::io::Error>().is_some());
    }
}
