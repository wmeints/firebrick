//! Port forwards of the working directory's sandbox: `fbk port forward` and `fbk port rm` change
//! them in the daemon right away and record them in `.firebrick.yml`.

use anyhow::{Result, anyhow, bail};
use firebrick_spec::PortMapping;
use std::path::Path;
use tonic::Code;
use tonic::transport::Channel;

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use crate::api::{ForwardPortRequest, PortForward, RemovePortRequest};
use crate::manage::{self, SPEC_FILE_NAME};
use crate::validate;

/// Forwards the host port to the guest port of the working directory's sandbox, replacing the
/// forward of the same host port, and records it in the spec file. The spec file only changes
/// once the daemon accepted the forward.
pub async fn forward(
    port: PortMapping,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let spec = SpecFile::read(working_dir, client).await?;
    let edited = firebrick_spec::add_port(&spec.text, port).map_err(validate::spec_error)?;

    let running = match manage::sandbox_status(client, &spec.name).await? {
        Some(_) => Some(send_forward(&spec.name, port, client).await?),
        None => None,
    };

    spec.write(&edited)?;
    print_outcome(&spec.name, running, || {
        format!(
            "Forwarding localhost:{} -> sandbox port {}",
            port.host, port.guest
        )
    });
    manage::warn_if_outdated(client, &spec.name).await;

    Ok(())
}

/// Removes the forward of the host port from the working directory's sandbox and from the
/// spec file. The spec file only changes once the daemon removed the forward.
pub async fn remove(
    host: u16,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let spec = SpecFile::read(working_dir, client).await?;
    let edited = firebrick_spec::remove_port(&spec.text, host).map_err(validate::spec_error)?;

    let running = match manage::sandbox_status(client, &spec.name).await? {
        Some(_) => Some(send_remove(&spec.name, host, client).await?),
        None if edited.is_some() => None,
        None => bail!(not_forwarded(host, &spec.name)),
    };

    if let Some(edited) = edited {
        spec.write(&edited)?;
    }

    print_outcome(&spec.name, running, || {
        format!("Stopped forwarding localhost:{host}")
    });
    manage::warn_if_outdated(client, &spec.name).await;

    Ok(())
}

/// Asks the daemon to forward the port and returns whether the sandbox runs.
async fn send_forward(
    name: &str,
    port: PortMapping,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<bool> {
    let request = ForwardPortRequest {
        name: name.to_string(),
        port: Some(PortForward {
            host: port.host.into(),
            guest: port.guest.into(),
        }),
    };

    match client.forward_port(request).await {
        Ok(response) => Ok(response.into_inner().running),
        Err(status) if status.code() == Code::FailedPrecondition => Err(anyhow!(
            "couldn't forward localhost:{}: {}",
            port.host,
            status.message()
        )),
        Err(status) => Err(status.into()),
    }
}

/// Asks the daemon to remove the forward of the host port and returns whether the sandbox runs.
async fn send_remove(
    name: &str,
    host: u16,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<bool> {
    let request = RemovePortRequest {
        name: name.to_string(),
        host: host.into(),
    };

    match client.remove_port(request).await {
        Ok(response) => Ok(response.into_inner().running),
        Err(status) if status.code() == Code::NotFound => Err(anyhow!(not_forwarded(host, name))),
        Err(status) => Err(status.into()),
    }
}

fn not_forwarded(host: u16, name: &str) -> String {
    format!("port {host} isn't forwarded for sandbox {name}")
}

/// Prints what happens to the change: `applied` when the sandbox runs, or when it takes
/// effect otherwise. `running` is `None` when the sandbox doesn't exist.
fn print_outcome(name: &str, running: Option<bool>, applied: impl FnOnce() -> String) {
    match running {
        Some(true) => println!("{}", applied()),
        Some(false) => println!("Sandbox {name} isn't running; the port applies when it starts."),
        None => println!("Sandbox {name} doesn't exist yet; the port applies when it's created."),
    }
}

/// The spec file of the working directory and the name of its sandbox. Without a spec file,
/// its text is a spec with only the name, so a new file is created on write.
struct SpecFile<'a> {
    working_dir: &'a Path,
    name: String,
    text: String,
}

impl<'a> SpecFile<'a> {
    /// Reads the spec file, or resolves the sandbox name the way `fbk start` does when there is
    /// none. Fails with the spec's diagnostic when the file is invalid, and when the sandbox
    /// belongs to another directory.
    async fn read(
        working_dir: &'a Path,
        client: &mut SandboxManagementServiceClient<Channel>,
    ) -> Result<Self> {
        let path = working_dir.join(SPEC_FILE_NAME);

        let (name, text) = match firebrick_spec::read_text(&path).map_err(validate::spec_error)? {
            Some(text) => (
                firebrick_spec::from_str(&text)
                    .map_err(validate::spec_error)?
                    .name,
                text,
            ),
            None => {
                let name = manage::resolve_spec(working_dir, client).await?.name;
                let text = format!("name: {name}\n");
                (name, text)
            }
        };

        manage::check_workspace_owner(&name, working_dir, client).await?;

        Ok(Self {
            working_dir,
            name,
            text,
        })
    }

    fn write(&self, text: &str) -> Result<()> {
        let path = self.working_dir.join(SPEC_FILE_NAME);

        Ok(firebrick_spec::write_text(&path, text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon::{self, OTHER_WORKSPACE};
    use crate::symlinked_spec;
    use std::fs;
    use tempfile::TempDir;

    const SPEC: &str = "name: dev\nports:\n  - 8080:80\n";

    /// Returns a working directory whose spec file names `dev` and forwards port 8080.
    fn workspace() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), SPEC).unwrap();
        dir
    }

    fn spec_text(dir: &TempDir) -> String {
        fs::read_to_string(dir.path().join(SPEC_FILE_NAME)).unwrap()
    }

    #[tokio::test]
    async fn forward_and_remove_refuse_sandbox_of_other_workspace() {
        let dir = workspace();
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;
        let port = PortMapping {
            host: 3000,
            guest: 3000,
        };

        let forward_result = forward(port, dir.path(), &mut client).await;
        let remove_result = remove(8080, dir.path(), &mut client).await;

        fake_daemon::assert_dev_belongs_to_other(forward_result);
        fake_daemon::assert_dev_belongs_to_other(remove_result);
        assert_eq!(spec_text(&dir), SPEC);
    }

    #[tokio::test]
    async fn remove_changes_spec_of_sandbox_that_does_not_exist_yet() {
        let dir = workspace();
        let mut client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        remove(8080, dir.path(), &mut client).await.unwrap();

        assert!(!spec_text(&dir).contains("8080"), "{}", spec_text(&dir));
    }

    async fn assert_forward_and_remove_refuse_symlink(content: Option<&str>) {
        let Some((dir, target)) = symlinked_spec::workspace(content) else {
            return;
        };
        // The sandbox belongs to this directory, so only the symlink check stops the write.
        let workspace = dunce::canonicalize(dir.path()).unwrap();
        let mut client = fake_daemon::serve("dev", workspace.to_str().unwrap()).await;
        let port = PortMapping {
            host: 3000,
            guest: 3000,
        };

        let forward_err = forward(port, dir.path(), &mut client).await.unwrap_err();
        let remove_err = remove(8080, dir.path(), &mut client).await.unwrap_err();

        assert_eq!(forward_err.to_string(), symlinked_spec::SYMLINK_ERROR);
        assert_eq!(remove_err.to_string(), symlinked_spec::SYMLINK_ERROR);
        assert_eq!(symlinked_spec::target_content(&target).as_deref(), content);
    }

    #[tokio::test]
    async fn forward_and_remove_refuse_a_symlink_to_a_spec_and_leave_it_alone() {
        assert_forward_and_remove_refuse_symlink(Some(SPEC)).await;
    }

    #[tokio::test]
    async fn forward_and_remove_refuse_a_dangling_symlink_and_create_no_file() {
        assert_forward_and_remove_refuse_symlink(None).await;
    }
}
