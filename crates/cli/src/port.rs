//! Port forwards of the working directory's sandbox: `fbk port forward` and `fbk port rm` change
//! them in the daemon right away and record them in `.firebrick.yml`.

use anyhow::{Context, Result, anyhow, bail};
use firebrick_spec::PortMapping;
use std::fs;
use std::io::ErrorKind;
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
    let edited = firebrick_spec::add_port(&spec.text, port).map_err(spec_error)?;

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
    let edited = firebrick_spec::remove_port(&spec.text, host).map_err(spec_error)?;

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

/// Turns an invalid spec into the diagnostic `fbk validate` prints.
fn spec_error(err: firebrick_spec::SandboxSpecError) -> anyhow::Error {
    anyhow!(validate::report(&err))
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

        let (name, text) = match fs::read_to_string(&path) {
            Ok(text) => (
                firebrick_spec::from_str(&text).map_err(spec_error)?.name,
                text,
            ),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let name = manage::resolve_spec(working_dir, client).await?.name;
                let text = format!("name: {name}\n");
                (name, text)
            }
            Err(err) => return Err(err).with_context(|| format!("can't read {}", path.display())),
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

        fs::write(&path, text).with_context(|| format!("can't write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon;
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
        let mut client = fake_daemon::serve("dev", "/home/user/other-project").await;
        let port = PortMapping {
            host: 3000,
            guest: 3000,
        };

        let forward_error = forward(port, dir.path(), &mut client).await.unwrap_err();
        let remove_error = remove(8080, dir.path(), &mut client).await.unwrap_err();

        assert!(
            forward_error.to_string().contains("belongs to"),
            "{forward_error}"
        );
        assert!(
            remove_error.to_string().contains("belongs to"),
            "{remove_error}"
        );
        assert_eq!(spec_text(&dir), SPEC);
    }

    #[tokio::test]
    async fn remove_changes_spec_of_sandbox_that_does_not_exist_yet() {
        let dir = workspace();
        let mut client = fake_daemon::serve("other", "/home/user/other-project").await;

        remove(8080, dir.path(), &mut client).await.unwrap();

        assert!(!spec_text(&dir).contains("8080"), "{}", spec_text(&dir));
    }
}
