use anyhow::{Context, Result, bail};
use std::future::Future;
use std::path::{Path, PathBuf};
use tonic::transport::Channel;

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use crate::sandbox;
use crate::spec_file::SPEC_FILE_NAME;

/// A client of the daemon's gRPC API.
type Client = SandboxManagementServiceClient<Channel>;

/// The spec file `fbk init` wrote.
#[derive(Debug)]
pub struct CreatedSpec {
    /// Path of the written `.firebrick.yml`.
    pub path: PathBuf,
    /// Name of the workspace's existing sandbox the spec was named after, if any.
    pub existing_sandbox: Option<String>,
}

/// Writes the default spec to `.firebrick.yml` in the working directory. The spec keeps the
/// name of the workspace's existing sandbox, so `fbk start` goes on using that sandbox, and is
/// otherwise named after the working directory. Refuses to replace an existing file unless
/// `force` is set, before awaiting `connect`, and refuses a symlink either way.
pub async fn init_spec(
    working_dir: &Path,
    force: bool,
    connect: impl Future<Output = Result<Client>>,
) -> Result<CreatedSpec> {
    let spec_path = working_dir.join(SPEC_FILE_NAME);
    let exists = firebrick_spec::exists(&spec_path)?;

    if exists && !force {
        bail!(
            "{SPEC_FILE_NAME} already exists in {}; use fbk init --force to overwrite it",
            working_dir.display()
        );
    }

    let existing_sandbox = if exists {
        sandbox_of_spec(&spec_path, working_dir, connect).await?
    } else {
        sandbox_without_spec(working_dir, connect).await?
    };
    let name = existing_sandbox
        .clone()
        .unwrap_or_else(|| spec_name(working_dir));
    let spec = firebrick_spec::default_spec(name);
    let content = serde_yaml::to_string(&spec).context("can't serialize the default spec")?;

    firebrick_spec::write_text(&spec_path, &content)?;

    Ok(CreatedSpec {
        path: spec_path,
        existing_sandbox,
    })
}

/// Returns the sandbox the spec being replaced names, when it exists and belongs to the working
/// directory. A spec without a readable name names no sandbox, so replacing it doesn't need the
/// daemon.
async fn sandbox_of_spec(
    spec_path: &Path,
    working_dir: &Path,
    connect: impl Future<Output = Result<Client>>,
) -> Result<Option<String>> {
    let Some(name) = name_in_spec(spec_path)? else {
        return Ok(None);
    };
    let mut client = connect_to_daemon(connect).await?;

    existing_sandbox(name, working_dir, &mut client).await
}

/// Returns the sandbox `fbk start` uses for a workspace without a spec, when it exists and
/// belongs to the working directory.
async fn sandbox_without_spec(
    working_dir: &Path,
    connect: impl Future<Output = Result<Client>>,
) -> Result<Option<String>> {
    let mut client = connect_to_daemon(connect).await?;
    let name = sandbox::resolve_spec(working_dir, &mut client).await?.name;

    existing_sandbox(name, working_dir, &mut client).await
}

/// Awaits the connection to the daemon.
async fn connect_to_daemon(connect: impl Future<Output = Result<Client>>) -> Result<Client> {
    connect
        .await
        .context("failed to connect to the daemon to check for an existing sandbox")
}

/// Reads the `name` of the spec file without validating the rest, so a spec this version of
/// `fbk` rejects still keeps its sandbox. Returns `None` when the file has no string `name`.
fn name_in_spec(spec_path: &Path) -> Result<Option<String>> {
    let text = firebrick_spec::read_text(spec_path)?.unwrap_or_default();

    // YAML that doesn't parse names no sandbox, so `--force` replaces it with the default.
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return Ok(None);
    };

    Ok(value
        .get("name")
        .and_then(serde_yaml::Value::as_str)
        .map(str::to_string))
}

/// Returns `name` when that sandbox exists and belongs to the working directory, and `None`
/// when it doesn't exist. Fails when it belongs to another directory.
async fn existing_sandbox(
    name: String,
    working_dir: &Path,
    client: &mut Client,
) -> Result<Option<String>> {
    let status = sandbox::sandbox_status(client, &name)
        .await
        .with_context(|| format!("failed to check for existing sandbox {name}"))?;

    if status.is_none() {
        return Ok(None);
    }

    sandbox::check_workspace_owner(&name, working_dir, client).await?;

    Ok(Some(name))
}

/// Names the sandbox after the working directory's leaf, cut to the longest name microsandbox
/// accepts. The label is ASCII, so each char is one byte.
fn spec_name(working_dir: &Path) -> String {
    let name: String = firebrick_utils::sanitize_label(working_dir)
        .chars()
        .take(firebrick_spec::MAX_SANDBOX_NAME_BYTES)
        .collect();

    name.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec_file::{default_name, legacy_name};
    use crate::test_support::fake_daemon::{self, OTHER_WORKSPACE};
    use crate::test_support::symlinked_spec;
    use anyhow::anyhow;
    use std::fs;
    use tempfile::TempDir;

    /// Creates the directory `name` in a new temporary directory and returns both, the
    /// directory canonicalized the way the daemon stores a sandbox's workspace.
    fn project_dir(name: &str) -> (TempDir, PathBuf) {
        let root = TempDir::new().unwrap();
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        let dir = dunce::canonicalize(dir).unwrap();
        (root, dir)
    }

    /// Runs `init_spec` against a daemon without sandboxes.
    async fn init_without_sandboxes(dir: &Path, force: bool) -> Result<CreatedSpec> {
        let client = fake_daemon::serve_unversioned().await;

        init_spec(dir, force, async { Ok(client) }).await
    }

    /// Runs `init_spec` against a daemon with the sandbox `name` that mounts `workspace`.
    async fn init_with_sandbox(
        dir: &Path,
        force: bool,
        name: &str,
        workspace: &Path,
    ) -> Result<CreatedSpec> {
        let client = fake_daemon::serve(name, workspace.to_str().unwrap()).await;

        init_spec(dir, force, async { Ok(client) }).await
    }

    /// Runs `init_spec` with a daemon that can't be reached.
    async fn init_without_daemon(dir: &Path, force: bool) -> Result<CreatedSpec> {
        init_spec(dir, force, async { Err(anyhow!("no daemon")) }).await
    }

    fn written_name(created: &CreatedSpec) -> String {
        firebrick_spec::from_file(&created.path)
            .unwrap()
            .unwrap()
            .name
    }

    /// Asserts that the file at `spec_path` holds the default spec named `name`, without an
    /// image, and that `fbk validate` accepts it.
    fn assert_default_spec(spec_path: &Path, name: &str) {
        let expected = serde_yaml::to_string(&firebrick_spec::default_spec(name.to_string()));

        assert_eq!(fs::read_to_string(spec_path).unwrap(), expected.unwrap());
        assert_eq!(
            firebrick_spec::from_file(spec_path).unwrap().unwrap().image,
            None
        );
        assert!(
            crate::spec_file::validate::validate_spec(spec_path.parent().unwrap()).unwrap(),
            "{name}"
        );
    }

    #[tokio::test]
    async fn creates_default_spec() {
        let (_root, dir) = project_dir("project");

        let created = init_without_sandboxes(&dir, false).await.unwrap();
        let spec_path = created.path;
        let content = fs::read_to_string(&spec_path).unwrap();
        let spec = firebrick_spec::from_file(&spec_path).unwrap().unwrap();

        assert_eq!(spec_path, dir.join(SPEC_FILE_NAME));
        assert_eq!(created.existing_sandbox, None);
        assert_eq!(spec.name, "project");
        assert_eq!(spec.image, None);
        assert_eq!(spec.init, Some(true));
        assert_eq!(spec.mise.and_then(|mise| mise.enabled), Some(true));
        assert!(content.contains("mise:\n  enabled: true\n"), "{content}");
        assert!(!content.contains("mounts"), "{content}");
    }

    #[tokio::test]
    async fn keeps_the_name_of_the_workspace_sandbox() {
        let (_root, dir) = project_dir("project");
        // The names `fbk start` gives a workspace without a spec; the tests in `spec_file`
        // check them against literal names.
        let hashed = default_name(&dir, false);
        let legacy = legacy_name(&dir).unwrap();

        assert_keeps_name(&dir, &hashed).await;
        assert_keeps_name(&dir, &legacy).await;
    }

    /// Asserts that `init_spec` names the spec after the workspace's sandbox `name`.
    async fn assert_keeps_name(dir: &Path, name: &str) {
        let created = init_with_sandbox(dir, false, name, dir).await.unwrap();

        assert_eq!(created.existing_sandbox.as_deref(), Some(name));
        assert_default_spec(&created.path, name);
        fs::remove_file(&created.path).unwrap();
    }

    #[tokio::test]
    async fn refuses_the_sandbox_of_another_workspace_without_writing() {
        let (_root, dir) = project_dir("project");
        let hashed = default_name(&dir, false);

        let error = init_with_sandbox(&dir, false, &hashed, Path::new(OTHER_WORKSPACE))
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with(&format!("sandbox {hashed} belongs to {OTHER_WORKSPACE};")),
            "{error}"
        );
        assert!(!dir.join(SPEC_FILE_NAME).exists());
    }

    #[tokio::test]
    async fn fails_without_daemon_and_writes_no_file() {
        let (_root, dir) = project_dir("project");

        let error = init_without_daemon(&dir, false).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            "failed to connect to the daemon to check for an existing sandbox"
        );
        assert_eq!(error.root_cause().to_string(), "no daemon");
        assert!(!dir.join(SPEC_FILE_NAME).exists());
    }

    #[tokio::test]
    async fn name_is_sanitized_leaf_of_working_dir() {
        assert_leaf_name("My.App", "my-app").await;
        assert_leaf_name("my project_", "my-project").await;
        // A long name is cut to a valid sandbox name.
        assert_leaf_name(&format!("{}_yz", "x".repeat(127)), &"x".repeat(127)).await;
        assert_leaf_name("__", "sandbox").await;
    }

    /// Asserts that `init_spec` in the directory `leaf` names the spec `name`.
    async fn assert_leaf_name(leaf: &str, name: &str) {
        let (_root, dir) = project_dir(leaf);

        let created = init_without_sandboxes(&dir, false).await.unwrap();

        assert_eq!(written_name(&created), name, "{leaf}");
    }

    #[tokio::test]
    async fn refuses_to_overwrite_existing_spec_without_connecting() {
        let (_root, dir) = project_dir("project");
        let spec_path = dir.join(SPEC_FILE_NAME);
        fs::write(&spec_path, "name: mine\n").unwrap();

        let err = init_without_daemon(&dir, false).await.unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                ".firebrick.yml already exists in {}; use fbk init --force to overwrite it",
                dir.display()
            )
        );
        assert_eq!(fs::read_to_string(&spec_path).unwrap(), "name: mine\n");
    }

    const PINNED_SPEC: &str = "name: mine\nimage: ubuntu:24.04\n# a long trailing comment\n";

    #[tokio::test]
    async fn force_overwrites_existing_spec() {
        let (_root, dir) = project_dir("project");
        let spec_path = dir.join(SPEC_FILE_NAME);
        fs::write(&spec_path, PINNED_SPEC).unwrap();

        let created = init_without_sandboxes(&dir, true).await.unwrap();

        assert_eq!(created.existing_sandbox, None);
        assert_default_spec(&spec_path, "project");
    }

    #[tokio::test]
    async fn force_replaces_a_spec_without_a_name_without_the_daemon() {
        assert_force_replaces_unnamed("name: [unclosed\n").await;
        assert_force_replaces_unnamed("image: ubuntu:24.04\n").await;
    }

    /// Asserts that `init_spec --force` replaces a spec with `content`, which names no sandbox,
    /// by the default spec named after the directory without connecting to the daemon.
    async fn assert_force_replaces_unnamed(content: &str) {
        let (_root, dir) = project_dir("project");
        let spec_path = dir.join(SPEC_FILE_NAME);
        fs::write(&spec_path, content).unwrap();

        let created = init_without_daemon(&dir, true).await.unwrap();

        assert_eq!(created.existing_sandbox, None, "{content}");
        assert_default_spec(&spec_path, "project");
    }

    #[tokio::test]
    async fn force_keeps_the_name_of_the_spec_sandbox() {
        assert_force_keeps_name(PINNED_SPEC).await;
        // A spec this version rejects still names its sandbox.
        assert_force_keeps_name("name: mine\nunknown: true\n").await;
    }

    /// Asserts that `init_spec --force` replaces a spec with `content`, which names the
    /// workspace's sandbox `mine`, by the default spec named `mine`.
    async fn assert_force_keeps_name(content: &str) {
        let (_root, dir) = project_dir("project");
        let spec_path = dir.join(SPEC_FILE_NAME);
        fs::write(&spec_path, content).unwrap();

        let created = init_with_sandbox(&dir, true, "mine", &dir).await.unwrap();

        assert_eq!(
            created.existing_sandbox.as_deref(),
            Some("mine"),
            "{content}"
        );
        assert_default_spec(&spec_path, "mine");
    }

    #[tokio::test]
    async fn force_refuses_the_spec_sandbox_of_another_workspace() {
        let (_root, dir) = project_dir("project");
        let spec_path = dir.join(SPEC_FILE_NAME);
        fs::write(&spec_path, PINNED_SPEC).unwrap();

        let error = init_with_sandbox(&dir, true, "mine", Path::new(OTHER_WORKSPACE))
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with(&format!("sandbox mine belongs to {OTHER_WORKSPACE};")),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&spec_path).unwrap(), PINNED_SPEC);
    }

    #[tokio::test]
    async fn refuses_a_symlinked_spec_and_leaves_its_target_alone() {
        assert_refuses_symlinked_spec(Some("notes\n"), false).await;
        assert_refuses_symlinked_spec(Some("notes\n"), true).await;
        assert_refuses_symlinked_spec(None, false).await;
        assert_refuses_symlinked_spec(None, true).await;
    }

    /// Asserts that `init_spec` refuses a `.firebrick.yml` that links to a file with
    /// `content`, or a missing one, and leaves that file alone.
    async fn assert_refuses_symlinked_spec(content: Option<&str>, force: bool) {
        let Some((dir, target)) = symlinked_spec::workspace(content) else {
            return;
        };

        let err = init_without_daemon(dir.path(), force).await.unwrap_err();

        assert_eq!(err.to_string(), symlinked_spec::SYMLINK_ERROR);
        assert_eq!(symlinked_spec::target_content(&target).as_deref(), content);
    }

    #[tokio::test]
    async fn write_failure_names_the_path() {
        // A directory that doesn't exist fails the write on every platform, also for root,
        // whom a read-only directory doesn't stop.
        let root = TempDir::new().unwrap();
        let dir = root.path().join("missing");

        let err = init_without_sandboxes(&dir, false).await.unwrap_err();
        assert!(
            err.to_string()
                .contains(&dir.join(SPEC_FILE_NAME).display().to_string()),
            "{err}"
        );
        assert!(err.root_cause().downcast_ref::<std::io::Error>().is_some());
    }
}
