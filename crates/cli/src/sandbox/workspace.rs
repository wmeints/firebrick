//! The sandbox of the working directory: resolves its spec and name, checks that a sandbox
//! belongs to the working directory and warns when its image is outdated.

use anyhow::{Context, Result, anyhow, bail};
use firebrick_spec::SandboxSpec;
use std::path::{Path, PathBuf};
use tonic::Code;
use tonic::transport::Channel;

use crate::api::{
    GetSandboxRequest, GetSandboxResponse, SandboxStatus,
    sandbox_management_service_client::SandboxManagementServiceClient,
};
use crate::spec_file::{SPEC_FILE_NAME, default_name, legacy_name, read_spec_file};

/// Prints a warning to stderr when the working directory's sandbox runs an older
/// `firebrick-base` image than the daemon's. Skips the warning when the sandbox can't be looked
/// up, so it never changes the command's outcome.
pub(crate) async fn warn_if_outdated(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) {
    if let Some(warning) = outdated_warning_of(client, name).await {
        eprintln!("{warning}");
    }
}

/// Looks up the working directory's sandbox and returns its warning from [`outdated_warning`],
/// or `None` when the lookup fails.
async fn outdated_warning_of(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> Option<String> {
    let request = GetSandboxRequest {
        name: name.to_string(),
    };
    let sandbox = client.get_sandbox(request).await.ok()?.into_inner();

    outdated_warning(&sandbox, false)
}

/// Returns the warning for a sandbox that the daemon reports as outdated, or `None` when it's
/// up to date. It points at `fbk upgrade`, with the sandbox's name when the command was `named`
/// it. When the spec in the sandbox's workspace pins the image the sandbox runs, `fbk upgrade`
/// would keep that image, so the warning asks to change the spec first.
pub(super) fn outdated_warning(sandbox: &GetSandboxResponse, named: bool) -> Option<String> {
    if sandbox.upgrade_image.is_empty() {
        return None;
    }

    let name = &sandbox.name;
    let command = if named {
        format!("fbk upgrade {name}")
    } else {
        "fbk upgrade".to_string()
    };
    let hint = match pinned_spec(sandbox) {
        Some(spec_path) => format!(
            "Set `image` in {} to {} or remove it, then run `{command}` to upgrade it.",
            spec_path.display(),
            sandbox.upgrade_image
        ),
        None => format!("Run `{command}` to upgrade it."),
    };

    Some(format!(
        "warning: sandbox {name} runs {}, but this firebrick release uses {}. {hint}",
        sandbox.image, sandbox.upgrade_image
    ))
}

/// Returns the path of the spec file in the sandbox's workspace when it names the image the
/// sandbox runs. A missing or unreadable spec pins nothing.
fn pinned_spec(sandbox: &GetSandboxResponse) -> Option<PathBuf> {
    if sandbox.workspace_host_path.is_empty() {
        return None;
    }

    let workspace = Path::new(&sandbox.workspace_host_path);
    let image = read_spec_file(workspace).ok()??.image?;

    (image == sandbox.image).then(|| workspace.join(SPEC_FILE_NAME))
}

/// Fails when the sandbox exists and mounts another host directory than the workspace, so two
/// directories whose names collide can't share a sandbox. Does nothing when the sandbox doesn't
/// exist or has no workspace mount.
pub(crate) async fn check_workspace_owner(
    name: &str,
    workspace: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let request = GetSandboxRequest {
        name: name.to_string(),
    };

    let host_path = match client.get_sandbox(request).await {
        Ok(response) => response.into_inner().workspace_host_path,
        Err(status) if status.code() == Code::NotFound => return Ok(()),
        Err(status) => {
            return Err(anyhow::Error::from(status).context(format!(
                "failed to look up sandbox {name} to check its workspace"
            )));
        }
    };

    if host_path.is_empty() {
        return Ok(());
    }

    // microsandbox stores the mounted host path canonicalized, so compare it canonicalized.
    let working_dir = dunce::canonicalize(workspace).with_context(|| {
        format!(
            "failed to resolve working directory {}",
            workspace.display()
        )
    })?;

    ensure_same_workspace(name, &host_path, &working_dir)
}

/// Fails when the sandbox mounts a host directory other than the canonical working directory.
/// An empty host path means the sandbox has no workspace mount and always passes.
fn ensure_same_workspace(name: &str, host_path: &str, working_dir: &Path) -> Result<()> {
    if host_path.is_empty() || Path::new(host_path) == working_dir {
        return Ok(());
    }

    bail!(
        "sandbox {name} belongs to {host_path}; give this directory its own name in \
         {SPEC_FILE_NAME} to use a separate sandbox"
    )
}

/// Returns the status of the sandbox, or `None` when it doesn't exist.
pub(crate) async fn sandbox_status(
    client: &mut SandboxManagementServiceClient<Channel>,
    name: &str,
) -> Result<Option<SandboxStatus>> {
    let request = GetSandboxRequest {
        name: name.to_string(),
    };

    match client.get_sandbox(request).await {
        Ok(response) => Ok(Some(response.into_inner().status())),
        Err(status) if status.code() == Code::NotFound => Ok(None),
        Err(status) => Err(status.into()),
    }
}

/// Returns the explicit sandbox name, or the name of the sandbox for the working directory.
/// The spec file is only read, and the sandbox's workspace only checked, without an explicit
/// name.
pub(super) async fn sandbox_name(
    name: Option<String>,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<String> {
    match name {
        Some(name) => Ok(name),
        None => Ok(resolve_owned_spec(working_dir, client).await?.name),
    }
}

/// Turns a `NotFound` status into an error that names the missing sandbox.
pub(crate) fn describe_status(name: &str, status: tonic::Status) -> anyhow::Error {
    if status.code() == Code::NotFound {
        return anyhow!("sandbox {name} doesn't exist");
    }

    status.into()
}

/// Loads the sandbox spec from the working directory, falling back to a default spec when
/// there's no spec file. The default spec keeps the name of a sandbox created before names
/// were hashed, and is otherwise named after the hash of the working directory path.
pub(crate) async fn resolve_spec(
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<SandboxSpec> {
    if let Some(spec) = read_spec_file(working_dir)? {
        return Ok(spec);
    }

    let legacy_exists = match legacy_name(working_dir) {
        Some(name) => sandbox_status(client, &name)
            .await
            .with_context(|| format!("failed to check for existing sandbox {name}"))?
            .is_some(),
        None => false,
    };

    Ok(firebrick_spec::default_spec(default_name(
        working_dir,
        legacy_exists,
    )))
}

/// Loads the spec of the working directory's sandbox like [`resolve_spec`], and fails when the
/// sandbox it names belongs to another directory, so a command without a sandbox name can't
/// change another project's sandbox.
pub(crate) async fn resolve_owned_spec(
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<SandboxSpec> {
    let spec = resolve_spec(working_dir, client).await?;
    check_workspace_owner(&spec.name, working_dir, client).await?;

    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::tests::{workspace_named, workspace_with_image};
    use crate::sandbox::{remove_sandbox, stop_sandbox};
    use crate::test_support::fake_daemon::{self, OTHER_WORKSPACE};
    use crate::test_support::symlinked_spec;
    use std::fs;
    use tempfile::TempDir;

    const OLD_BASE: &str = "ghcr.io/wmeints/firebrick-base:v0.7.0";
    const NEW_BASE: &str = "ghcr.io/wmeints/firebrick-base:v0.8.0";

    /// Returns the sandbox `name`, outdated by [`NEW_BASE`], that mounts the workspace.
    fn outdated(name: &str, workspace: &str) -> GetSandboxResponse {
        GetSandboxResponse {
            name: name.to_string(),
            workspace_host_path: workspace.to_string(),
            image: OLD_BASE.to_string(),
            upgrade_image: NEW_BASE.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn outdated_warning_points_at_fbk_upgrade_with_the_name_it_was_given() {
        let sandbox = outdated("myproject-3f2a1c", "");
        let warning = |command: &str| {
            format!(
                "warning: sandbox myproject-3f2a1c runs {OLD_BASE}, but this firebrick release \
                 uses {NEW_BASE}. Run `{command}` to upgrade it."
            )
        };

        assert_eq!(
            outdated_warning(&sandbox, false),
            Some(warning("fbk upgrade"))
        );
        assert_eq!(
            outdated_warning(&sandbox, true),
            Some(warning("fbk upgrade myproject-3f2a1c"))
        );
    }

    #[test]
    fn outdated_warning_asks_to_change_a_spec_that_pins_the_old_image() {
        let dir = workspace_with_image("dev", OLD_BASE);
        let sandbox = outdated("dev", dir.path().to_str().unwrap());
        let spec_path = dir.path().join(SPEC_FILE_NAME);

        assert_eq!(
            outdated_warning(&sandbox, false),
            Some(format!(
                "warning: sandbox dev runs {OLD_BASE}, but this firebrick release uses \
                 {NEW_BASE}. Set `image` in {} to {NEW_BASE} or remove it, then run \
                 `fbk upgrade` to upgrade it.",
                spec_path.display()
            ))
        );
    }

    #[test]
    fn outdated_warning_points_at_fbk_upgrade_when_the_spec_names_another_image() {
        let dir = workspace_with_image("dev", "alpine:3.22");
        let sandbox = outdated("dev", dir.path().to_str().unwrap());

        let warning = outdated_warning(&sandbox, false).unwrap();

        assert!(
            warning.ends_with(". Run `fbk upgrade` to upgrade it."),
            "{warning}"
        );
    }

    #[test]
    fn up_to_date_sandbox_gets_no_warning() {
        let sandbox = GetSandboxResponse {
            upgrade_image: String::new(),
            ..outdated("dev", "")
        };

        assert_eq!(outdated_warning(&sandbox, false), None);
    }

    #[tokio::test]
    async fn outdated_check_warns_about_the_daemons_outdated_sandbox() {
        let mut client = fake_daemon::serve_outdated("dev", "").await;

        let warning = outdated_warning_of(&mut client, "dev").await;

        assert_eq!(
            warning,
            Some(format!(
                "warning: sandbox dev runs {}, but this firebrick release uses {}. Run \
                 `fbk upgrade` to upgrade it.",
                fake_daemon::OLD_IMAGE,
                fake_daemon::UPGRADED_IMAGE
            ))
        );
    }

    #[tokio::test]
    async fn outdated_check_skips_an_up_to_date_sandbox() {
        let mut client = fake_daemon::serve("dev", "").await;

        assert_eq!(outdated_warning_of(&mut client, "dev").await, None);
    }

    #[tokio::test]
    async fn outdated_check_skips_the_warning_when_the_lookup_fails() {
        let mut client = fake_daemon::serve_outdated("dev", "").await;

        let missing = outdated_warning_of(&mut client, "missing").await;
        let unreachable = outdated_warning_of(&mut unreachable_client(), "dev").await;

        assert_eq!((missing, unreachable), (None, None));
    }

    /// Returns a client for an address nothing listens on, so a test fails when it calls the
    /// daemon.
    fn unreachable_client() -> SandboxManagementServiceClient<Channel> {
        SandboxManagementServiceClient::new(
            Channel::from_static("http://127.0.0.1:1").connect_lazy(),
        )
    }

    #[tokio::test]
    async fn uses_name_from_spec_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), "name: dev\n").unwrap();

        let spec = resolve_spec(dir.path(), &mut unreachable_client())
            .await
            .unwrap();

        assert_eq!(spec.name, "dev");
    }

    #[tokio::test]
    async fn hashes_path_without_spec_file_or_legacy_name() {
        // The root path has no legacy name, so the daemon isn't asked whether one exists.
        let spec = resolve_spec(Path::new("/"), &mut unreachable_client())
            .await
            .unwrap();

        assert_eq!(spec.name, "firebrick-8a5eda");
    }

    #[tokio::test]
    async fn invalid_spec_file_returns_error() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), "image: ubuntu:24.04\n").unwrap();

        let result = resolve_spec(dir.path(), &mut unreachable_client()).await;

        assert!(result.is_err());
    }

    async fn assert_resolve_refuses_symlink(content: Option<&str>) {
        let Some((dir, _target)) = symlinked_spec::workspace(content) else {
            return;
        };

        let err = resolve_spec(dir.path(), &mut unreachable_client())
            .await
            .unwrap_err();

        assert_eq!(err.to_string(), symlinked_spec::SYMLINK_ERROR);
    }

    #[tokio::test]
    async fn symlinked_spec_file_returns_error() {
        assert_resolve_refuses_symlink(Some("name: dev\n")).await;
    }

    #[tokio::test]
    async fn dangling_symlinked_spec_file_returns_error() {
        assert_resolve_refuses_symlink(None).await;
    }

    #[tokio::test]
    async fn explicit_name_skips_spec_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), "image: ubuntu:24.04\n").unwrap();

        let name = sandbox_name(
            Some("other".to_string()),
            dir.path(),
            &mut unreachable_client(),
        )
        .await
        .unwrap();

        assert_eq!(name, "other");
    }

    #[tokio::test]
    async fn without_name_refuses_sandbox_of_other_workspace() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        fake_daemon::assert_dev_belongs_to_other(sandbox_name(None, dir.path(), &mut client).await);
        fake_daemon::assert_dev_belongs_to_other(stop_sandbox(None, dir.path(), &mut client).await);
        fake_daemon::assert_dev_belongs_to_other(
            remove_sandbox(None, true, dir.path(), &mut client).await,
        );
    }

    #[tokio::test]
    async fn without_name_accepts_sandbox_of_working_directory() {
        let dir = workspace_named("dev");
        let canonical = dunce::canonicalize(dir.path()).unwrap();
        let mut client = fake_daemon::serve("dev", canonical.to_str().unwrap()).await;

        let name = sandbox_name(None, dir.path(), &mut client).await.unwrap();

        assert_eq!(name, "dev");
    }

    #[tokio::test]
    async fn without_name_accepts_missing_sandbox() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        let name = sandbox_name(None, dir.path(), &mut client).await.unwrap();

        assert_eq!(name, "dev");
    }

    #[tokio::test]
    async fn without_name_accepts_sandbox_without_workspace_mount() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("dev", "").await;

        let name = sandbox_name(None, dir.path(), &mut client).await.unwrap();

        assert_eq!(name, "dev");
    }

    #[test]
    fn not_found_status_names_missing_sandbox() {
        let error = describe_status("dev", tonic::Status::not_found("couldn't find it"));

        assert_eq!(error.to_string(), "sandbox dev doesn't exist");
    }

    #[test]
    fn other_status_is_kept() {
        let error = describe_status("dev", tonic::Status::internal("failed to stop sandbox"));

        assert!(
            error.to_string().contains("failed to stop sandbox"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn failed_legacy_lookup_names_the_sandbox() {
        let dir = TempDir::new().unwrap();

        let error = resolve_spec(dir.path(), &mut unreachable_client())
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("failed to check for existing sandbox "),
            "{error}"
        );
    }

    #[test]
    fn same_workspace_passes() {
        let result = ensure_same_workspace(
            "dev",
            "/home/user/my-project",
            Path::new("/home/user/my-project"),
        );

        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn other_workspace_is_refused() {
        let error = ensure_same_workspace(
            "firebrick-d9f287",
            "/home/user/my-project",
            Path::new("/home/user/other-project"),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "sandbox firebrick-d9f287 belongs to /home/user/my-project; give this directory its \
             own name in .firebrick.yml to use a separate sandbox"
        );
    }

    #[test]
    fn sandbox_without_workspace_mount_passes() {
        let result = ensure_same_workspace("dev", "", Path::new("/home/user/other-project"));

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn resolve_spec_migrates_a_boolean_mise() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(SPEC_FILE_NAME);
        fs::write(&path, "name: dev\nmise: false\n").unwrap();

        let spec = resolve_spec(dir.path(), &mut unreachable_client())
            .await
            .unwrap();

        assert_eq!(spec.mise.and_then(|mise| mise.enabled), Some(false));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "name: dev\nmise:\n  enabled: false\n"
        );
    }
}
