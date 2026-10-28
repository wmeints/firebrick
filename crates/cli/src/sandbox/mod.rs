//! The sandboxes in the daemon: `fbk start`, `stop`, `rm`, `upgrade` and `ls`, and the sandbox
//! of the working directory that other commands act on.

pub mod git;
mod host_paths;
mod list;
mod start;
mod upgrade;
mod workspace;

use anyhow::{Result, anyhow};
use std::path::Path;
use tonic::Code;
use tonic::transport::Channel;

use crate::api::{
    RemoveSandboxRequest, StopSandboxRequest,
    sandbox_management_service_client::SandboxManagementServiceClient,
};

pub use list::{OutputFormat, list_sandboxes};
pub use start::start_sandbox;
pub(crate) use start::{ensure_running, network_policy};
pub use upgrade::upgrade_sandbox;
use workspace::sandbox_name;
pub(crate) use workspace::{
    check_workspace_owner, describe_status, resolve_owned_spec, resolve_spec, sandbox_status,
    warn_if_outdated,
};

/// Stops the named sandbox, or the sandbox for the working directory without a name.
pub async fn stop_sandbox(
    name: Option<String>,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let name = sandbox_name(name, working_dir, client).await?;

    client
        .stop_sandbox(StopSandboxRequest { name: name.clone() })
        .await
        .map_err(|status| describe_status(&name, status))?;

    Ok(())
}

/// Removes the named sandbox, or the sandbox for the working directory without a name. A
/// running sandbox is only removed with `force`, which stops it first.
pub async fn remove_sandbox(
    name: Option<String>,
    force: bool,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let name = sandbox_name(name, working_dir, client).await?;

    client
        .remove_sandbox(RemoveSandboxRequest {
            name: name.clone(),
            force,
        })
        .await
        .map_err(|status| describe_remove_status(&name, status))?;

    Ok(())
}

/// Turns a `FailedPrecondition` status of a remove into an error that explains how to remove
/// the running sandbox, and other statuses like [`describe_status`].
fn describe_remove_status(name: &str, status: tonic::Status) -> anyhow::Error {
    if status.code() == Code::FailedPrecondition {
        return anyhow!(
            "sandbox {name} is running. Stop it with `fbk stop`, or remove it with `fbk rm --force`."
        );
    }

    describe_status(name, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec_file::SPEC_FILE_NAME;
    use crate::test_support::fake_daemon::{self, OTHER_WORKSPACE};
    use std::fs;
    use tempfile::TempDir;

    /// Returns a working directory whose spec file names the sandbox `name`.
    pub(super) fn workspace_named(name: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), format!("name: {name}\n")).unwrap();
        dir
    }

    /// Returns a working directory whose spec file names the sandbox `name` and the image.
    pub(super) fn workspace_with_image(name: &str, image: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        let spec = format!("name: {name}\nimage: {image}\n");
        fs::write(dir.path().join(SPEC_FILE_NAME), spec).unwrap();
        dir
    }

    #[tokio::test]
    async fn stop_and_remove_with_explicit_name_skip_owner_check() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        let stop_error = stop_sandbox(Some("dev".to_string()), dir.path(), &mut client)
            .await
            .unwrap_err();
        let remove_error = remove_sandbox(Some("dev".to_string()), true, dir.path(), &mut client)
            .await
            .unwrap_err();

        assert!(
            stop_error
                .to_string()
                .contains(&fake_daemon::refusal("stop_sandbox", "dev")),
            "{stop_error}"
        );
        assert!(
            remove_error
                .to_string()
                .contains(&fake_daemon::refusal("remove_sandbox", "dev")),
            "{remove_error}"
        );
    }

    #[test]
    fn failed_precondition_on_remove_explains_force() {
        let error = describe_remove_status(
            "dev",
            tonic::Status::failed_precondition("sandbox dev is running"),
        );

        assert_eq!(
            error.to_string(),
            "sandbox dev is running. Stop it with `fbk stop`, or remove it with `fbk rm --force`."
        );
    }

    #[test]
    fn not_found_on_remove_names_missing_sandbox() {
        let error = describe_remove_status("dev", tonic::Status::not_found("couldn't find it"));

        assert_eq!(error.to_string(), "sandbox dev doesn't exist");
    }
}
