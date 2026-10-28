//! `fbk upgrade`: asks the daemon to upgrade a sandbox to the image of its workspace's spec,
//! showing the image pull while it runs.

use super::start::{Streamed, connect_instructions, follow_pull};
use super::workspace::{describe_status, resolve_owned_spec, sandbox_status};
use anyhow::{Context, Result, anyhow, bail};
use firebrick_spec::SandboxSpec;
use std::path::Path;
use tonic::Code;
use tonic::transport::Channel;

use crate::api::{
    GetSandboxRequest, SandboxStatus, SandboxUpgraded, UpgradeSandboxRequest,
    sandbox_management_service_client::SandboxManagementServiceClient, upgrade_sandbox_response,
};
use crate::spec_file::read_spec_file;

/// Recreates the named sandbox, or the sandbox for the working directory without a name, from
/// the latest version of its spec's image, or of the daemon's default image when the spec names
/// none. Shows the progress of the image pull, and the connect instructions when the upgraded
/// sandbox runs.
pub async fn upgrade_sandbox(
    name: Option<String>,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let request = upgrade_request(name, working_dir, client).await?;
    let name = request.name.clone();
    let upgraded = send_upgrade_request(request, client).await?;

    println!("{}", upgrade_message(&name, &upgraded));

    if !upgraded.upgraded {
        return Ok(());
    }

    let sandbox = client
        .get_sandbox(GetSandboxRequest { name: name.clone() })
        .await
        .with_context(|| {
            format!("upgraded sandbox {name}, but failed to read its connect instructions")
        })?
        .into_inner();

    if sandbox.status() == SandboxStatus::Running {
        print!("{}", connect_instructions(&sandbox));
    }

    Ok(())
}

/// Returns the request that upgrades the named sandbox to the image in the spec of its
/// workspace, or the working directory's sandbox to the image in the working directory's spec.
/// An empty image asks for the daemon's default image. Fails when the sandbox doesn't exist,
/// or, without a name, belongs to another directory, and when the spec is invalid.
async fn upgrade_request(
    name: Option<String>,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<UpgradeSandboxRequest> {
    let (name, image) = match name {
        Some(name) => {
            let image = workspace_image(&name, client).await?;
            (name, image)
        }
        None => {
            let spec = working_dir_sandbox_spec(working_dir, client).await?;
            (spec.name, spec.image)
        }
    };

    Ok(UpgradeSandboxRequest {
        name,
        image: image.unwrap_or_default(),
    })
}

/// Returns the image in the spec file of the named sandbox's workspace, or `None` when the spec
/// names none or the sandbox has no workspace or no spec file there. Fails when the sandbox
/// doesn't exist.
async fn workspace_image(
    name: &str,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<Option<String>> {
    let workspace = client
        .get_sandbox(GetSandboxRequest {
            name: name.to_string(),
        })
        .await
        .map_err(|status| describe_status(name, status))?
        .into_inner()
        .workspace_host_path;

    if workspace.is_empty() {
        return Ok(None);
    }

    Ok(read_spec_file(Path::new(&workspace))?.and_then(|spec| spec.image))
}

/// Returns the spec of the working directory's sandbox, failing when the sandbox doesn't exist
/// or belongs to another directory.
async fn working_dir_sandbox_spec(
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<SandboxSpec> {
    let spec = resolve_owned_spec(working_dir, client).await?;

    if sandbox_status(client, &spec.name).await?.is_none() {
        bail!(
            "sandbox {} doesn't exist; run fbk start to create it",
            spec.name
        );
    }

    Ok(spec)
}

/// Sends the upgrade request and shows the progress of the image pull while the daemon pulls
/// the image. Returns the message that ends the stream.
async fn send_upgrade_request(
    request: UpgradeSandboxRequest,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<SandboxUpgraded> {
    let stream = client
        .upgrade_sandbox(request)
        .await
        .map_err(daemon_error)?
        .into_inner();
    let upgraded = follow_pull(stream, |response| match response.message {
        Some(upgrade_sandbox_response::Message::PullProgress(progress)) => {
            Streamed::Progress(progress)
        }
        Some(upgrade_sandbox_response::Message::Upgraded(upgraded)) => Streamed::Done(upgraded),
        None => Streamed::Empty,
    })
    .await
    .map_err(daemon_error)?;

    upgraded.context("the daemon ended the upgrade without a result")
}

/// Turns a status the daemon sends on purpose for a request it refuses into an error with
/// just its message, and keeps other statuses, such as a broken connection, whole.
fn daemon_error(status: tonic::Status) -> anyhow::Error {
    match status.code() {
        Code::FailedPrecondition | Code::NotFound | Code::InvalidArgument => {
            anyhow!("{}", status.message())
        }
        _ => status.into(),
    }
}

/// Returns the message for the result of the upgrade of the sandbox.
fn upgrade_message(name: &str, upgraded: &SandboxUpgraded) -> String {
    if upgraded.upgraded {
        format!(
            "Upgraded sandbox {name} from {} to {}",
            upgraded.old_image, upgraded.new_image
        )
    } else {
        format!(
            "Sandbox {name} is already up to date ({})",
            upgraded.new_image
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::tests::{workspace_named, workspace_with_image};
    use crate::spec_file::SPEC_FILE_NAME;
    use crate::test_support::fake_daemon::{self, OTHER_WORKSPACE};
    use std::fs;
    use tempfile::TempDir;

    /// Serves a fake daemon whose sandbox `name` mounts the directory.
    async fn serve_sandbox_of(name: &str, dir: &Path) -> SandboxManagementServiceClient<Channel> {
        let canonical = dunce::canonicalize(dir).unwrap();
        fake_daemon::serve(name, canonical.to_str().unwrap()).await
    }

    fn upgrade(name: &str, image: &str) -> UpgradeSandboxRequest {
        UpgradeSandboxRequest {
            name: name.to_string(),
            image: image.to_string(),
        }
    }

    #[tokio::test]
    async fn upgrade_without_name_asks_for_the_image_of_the_working_directorys_spec() {
        let dir = workspace_with_image("dev", "alpine:3.22");
        let mut client = serve_sandbox_of("dev", dir.path()).await;

        let request = upgrade_request(None, dir.path(), &mut client)
            .await
            .unwrap();

        assert_eq!(request, upgrade("dev", "alpine:3.22"));
    }

    #[tokio::test]
    async fn upgrade_without_image_in_the_spec_asks_for_the_default_image() {
        let dir = workspace_named("dev");
        let mut client = serve_sandbox_of("dev", dir.path()).await;

        let request = upgrade_request(None, dir.path(), &mut client)
            .await
            .unwrap();

        assert_eq!(request, upgrade("dev", ""));
    }

    #[tokio::test]
    async fn upgrade_with_name_asks_for_the_image_of_the_sandboxs_workspace() {
        let workspace = workspace_with_image("project", "alpine:3.22");
        let working_dir = workspace_with_image("other", "debian:13");
        let mut client = serve_sandbox_of("dev", workspace.path()).await;

        let request = upgrade_request(Some("dev".to_string()), working_dir.path(), &mut client)
            .await
            .unwrap();

        assert_eq!(request, upgrade("dev", "alpine:3.22"));
    }

    #[tokio::test]
    async fn upgrade_with_name_asks_for_the_default_image_without_a_spec_file() {
        assert_eq!(named_upgrade_of("").await, upgrade("dev", ""));
        assert_eq!(
            named_upgrade_of("/nonexistent/firebrick-workspace").await,
            upgrade("dev", "")
        );
    }

    /// Returns the request that upgrades the sandbox `dev` by name, when it mounts the workspace
    /// and the working directory's spec names another image.
    async fn named_upgrade_of(workspace: &str) -> UpgradeSandboxRequest {
        let working_dir = workspace_with_image("other", "debian:13");
        let mut client = fake_daemon::serve("dev", workspace).await;

        upgrade_request(Some("dev".to_string()), working_dir.path(), &mut client)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn upgrade_with_name_fails_for_an_invalid_spec_in_the_workspace() {
        let workspace = TempDir::new().unwrap();
        let spec_path = workspace.path().join(SPEC_FILE_NAME);
        fs::write(&spec_path, "image: ubuntu:24.04\n").unwrap();
        let mut client = serve_sandbox_of("dev", workspace.path()).await;

        let error = upgrade_sandbox(Some("dev".to_string()), Path::new("/"), &mut client)
            .await
            .unwrap_err();

        let expected = firebrick_spec::from_file(&spec_path).unwrap_err();
        assert_eq!(error.to_string(), expected.to_string());
    }

    #[tokio::test]
    async fn upgrade_with_name_of_a_missing_sandbox_fails() {
        let mut client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        let error = upgrade_sandbox(Some("dev".to_string()), Path::new("/"), &mut client)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "sandbox dev doesn't exist");
    }

    #[tokio::test]
    async fn upgrade_without_name_of_a_missing_sandbox_tells_to_start_it() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        let error = upgrade_sandbox(None, dir.path(), &mut client)
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "sandbox dev doesn't exist; run fbk start to create it"
        );
    }

    #[tokio::test]
    async fn upgrade_without_name_refuses_sandbox_of_other_workspace() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        fake_daemon::assert_dev_belongs_to_other(
            upgrade_sandbox(None, dir.path(), &mut client).await,
        );
    }

    #[tokio::test]
    async fn upgrade_returns_the_daemons_result() {
        let mut client = fake_daemon::serve("dev", "").await;

        let up_to_date = send_upgrade_request(upgrade("dev", "alpine:3.22"), &mut client)
            .await
            .unwrap();
        let upgraded =
            send_upgrade_request(upgrade("dev", fake_daemon::UPGRADED_IMAGE), &mut client)
                .await
                .unwrap();

        assert_eq!(
            up_to_date,
            SandboxUpgraded {
                upgraded: false,
                old_image: "alpine:3.22".to_string(),
                new_image: "alpine:3.22".to_string(),
            }
        );
        assert_eq!(
            upgraded,
            SandboxUpgraded {
                upgraded: true,
                old_image: fake_daemon::OLD_IMAGE.to_string(),
                new_image: fake_daemon::UPGRADED_IMAGE.to_string(),
            }
        );
    }

    #[tokio::test]
    async fn upgrade_reads_the_upgraded_sandbox_afterwards() {
        let dir = workspace_with_image("dev", fake_daemon::UPGRADED_IMAGE);
        let mut client = serve_sandbox_of("dev", dir.path()).await;

        let result = upgrade_sandbox(None, dir.path(), &mut client).await;

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn upgrade_shows_the_daemons_error_message() {
        let mut client = fake_daemon::serve("dev", "").await;

        let error = send_upgrade_request(upgrade("other", ""), &mut client)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "couldn't find specified sandbox");
    }

    #[test]
    fn daemon_error_keeps_the_code_of_unexpected_statuses() {
        let refused = daemon_error(tonic::Status::failed_precondition("sandbox dev is paused"));
        let broken = daemon_error(tonic::Status::unknown("h2 protocol error"));

        assert_eq!(refused.to_string(), "sandbox dev is paused");
        assert_eq!(
            broken
                .downcast_ref::<tonic::Status>()
                .map(tonic::Status::code),
            Some(Code::Unknown)
        );
    }

    #[test]
    fn upgrade_message_names_the_old_and_new_image() {
        let upgraded = SandboxUpgraded {
            upgraded: true,
            old_image: "alpine:3.21".to_string(),
            new_image: "alpine:3.22".to_string(),
        };

        assert_eq!(
            upgrade_message("dev", &upgraded),
            "Upgraded sandbox dev from alpine:3.21 to alpine:3.22"
        );
    }
}
