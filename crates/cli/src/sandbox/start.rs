//! `fbk start`: builds the start request from the spec, asks the user to approve new
//! settings, and starts the sandbox or waits for it to run, showing the progress and how to
//! connect to it.

use super::git;
use super::host_paths::{mise_config, resolve_mounts};
use super::list::format_status;
use super::workspace::{check_workspace_owner, outdated_warning, resolve_spec, sandbox_status};
use anyhow::{Context, Result, bail};
use firebrick_spec::{MountSpec, NetworkRule, NetworkSpec, PortMapping, SandboxSpec};
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tonic::Streaming;
use tonic::transport::Channel;

use crate::api::{
    EnvVars, GetSandboxRequest, GetSandboxResponse, GitIdentity, ImagePullProgress,
    MiseInstallProgress, NetworkPolicy, PortForward, PortForwards, SandboxResources,
    SandboxStarted, SandboxStatus, SandboxVolumes, StartSandboxRequest,
    sandbox_management_service_client::SandboxManagementServiceClient, start_sandbox_response,
};
use crate::approval::{Approver, Clipboard, Settings};
use crate::spec_file::SPEC_FILE_NAME;
use crate::ui::progress::{MiseProgressView, PullProgressView};

/// Starts the named sandbox, or the sandbox for the working directory without a name. Does
/// nothing when it's already running, and waits for it when it's starting. Only the sandbox for
/// the working directory is created when it doesn't exist, because creating one needs its spec.
/// `assume_yes` approves changed mounts and network settings without asking.
pub async fn start_sandbox(
    name: Option<String>,
    working_dir: &Path,
    assume_yes: bool,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let named = name.is_some();
    let (name, started) = match name {
        Some(name) => start_named_sandbox(name, client).await?,
        None => start_working_dir_sandbox(working_dir, assume_yes, client).await?,
    };

    let sandbox = client
        .get_sandbox(GetSandboxRequest { name })
        .await?
        .into_inner();

    if let Some(warning) = outdated_warning(&sandbox, named) {
        eprintln!("{warning}");
    }
    print!("{}", forward_lines(&started));
    print!("{}", connect_instructions(&sandbox));

    Ok(())
}

/// Returns a line per open forward of the sandbox.
fn forward_lines(started: &SandboxStarted) -> String {
    started
        .forwards
        .iter()
        .map(|forward| {
            format!(
                "Forwarding localhost:{} -> sandbox port {}\n",
                forward.host, forward.guest
            )
        })
        .collect()
}

/// Prints a warning to stderr for each forward the daemon couldn't open.
pub(super) fn warn_failed_forwards(started: &SandboxStarted) {
    for failure in &started.failed_forwards {
        let host = failure.port.as_ref().map_or(0, |port| port.host);
        eprintln!(
            "warning: couldn't forward localhost:{host}: {}",
            failure.reason
        );
    }
}

/// Returns the commands that connect to the sandbox over SSH and open its workspace in VS Code
/// and Zed, one per line. The editor commands need both the host name and the workspace path.
pub(super) fn connect_instructions(sandbox: &GetSandboxResponse) -> String {
    let (hostname, workspace_path) = (&sandbox.hostname, &sandbox.workspace_path);

    match (hostname.is_empty(), workspace_path.is_empty()) {
        (true, _) => String::new(),
        (false, true) => format!("Connect with: ssh {hostname}\n"),
        (false, false) => format!(
            "Connect with: ssh {hostname}\n\
             Open in VS Code: code --folder-uri vscode-remote://ssh-remote+{hostname}{workspace_path}\n\
             Open in Zed: zed ssh://{hostname}{workspace_path}\n"
        ),
    }
}

/// Starts the existing sandbox with the name with its stored ports, and returns the name with
/// the daemon's response.
async fn start_named_sandbox(
    name: String,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<(String, SandboxStarted)> {
    // The daemon falls back to the sandbox name for the host name when the workspace is empty.
    let Some(started) = start_if_exists(start_request(&name, Path::new(""), None), client).await?
    else {
        bail!("sandbox {name} doesn't exist; run fbk start in its project directory to create it");
    };

    warn_failed_forwards(&started);

    Ok((name, started))
}

/// Starts the sandbox for the working directory, creating it when needed, and returns its name
/// with the daemon's response.
async fn start_working_dir_sandbox(
    working_dir: &Path,
    assume_yes: bool,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<(String, SandboxStarted)> {
    let spec = resolve_spec(working_dir, client).await?;
    let name = spec.name.clone();

    let started = ensure_running(spec, working_dir, assume_yes, client).await?;

    Ok((name, started))
}

/// Prints a notice to stderr when the running sandbox keeps other environment variables than
/// the spec sets until it restarts.
pub(super) fn warn_env_pending_restart(started: &SandboxStarted) {
    if started.env_pending_restart {
        eprintln!(
            "env changes in {SPEC_FILE_NAME} apply after the sandbox restarts: fbk stop && fbk start"
        );
    }
}

/// Makes sure the user approved the mounts, network and clipboard settings of the new sandbox,
/// asking on the terminal when they changed. `network` is the spec's network section, or its
/// default.
pub(super) async fn approve_new_sandbox(
    request: &StartSandboxRequest,
    network: NetworkSpec,
    workspace: &Path,
    assume_yes: bool,
) -> Result<()> {
    let settings = new_sandbox_settings(request, network);

    Approver::new(assume_yes)
        .run(workspace, move |approvals, prompt| {
            approvals.approve_sandbox(settings, prompt)
        })
        .await
}

/// Returns the settings a new sandbox needs approval for: the request's mounts, whose host
/// paths are canonical, so a symlink shows up as the directory it points to, with the read-only
/// mount of the mise dotfiles, and its clipboard setting.
fn new_sandbox_settings(request: &StartSandboxRequest, network: NetworkSpec) -> Settings {
    let dotfiles = (!request.mise_config.is_empty())
        .then(|| MountSpec::mise_dotfiles(request.mise_config.as_str()));

    Settings {
        mounts: request
            .mounts
            .iter()
            .map(MountSpec::from)
            .chain(dotfiles)
            .collect(),
        network,
        clipboard: Clipboard::shared(request.clipboard.unwrap_or(false)),
    }
}

/// Sends the start request and shows the progress of the image pull and of installing the mise
/// tools while the daemon starts the sandbox. Returns the message that ends the stream.
pub(super) async fn send_start_request(
    request: StartSandboxRequest,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<SandboxStarted> {
    let stream = client.start_sandbox(request).await?.into_inner();
    let started = follow_pull(stream, |response| match response.message {
        Some(start_sandbox_response::Message::PullProgress(progress)) => {
            Streamed::Progress(progress)
        }
        Some(start_sandbox_response::Message::MiseProgress(progress)) => Streamed::Mise(progress),
        Some(start_sandbox_response::Message::Started(started)) => Streamed::Done(started),
        None => Streamed::Empty,
    })
    .await?;

    started.context("the daemon ended the start without starting the sandbox")
}

/// Turns the ports of a spec into their API message.
fn port_forwards(ports: &[PortMapping]) -> PortForwards {
    PortForwards {
        ports: ports
            .iter()
            .map(|port| PortForward {
                host: port.host.into(),
                guest: port.guest.into(),
            })
            .collect(),
    }
}

/// Returns the request that starts an existing sandbox. The workspace lets the daemon name
/// sandboxes created before SSH support, and `None` ports keep the stored ones.
fn start_request(name: &str, workspace: &Path, ports: Option<PortForwards>) -> StartSandboxRequest {
    StartSandboxRequest {
        name: name.to_string(),
        workspace: workspace.to_string_lossy().into_owned(),
        ports,
        ..Default::default()
    }
}

/// Returns the request that starts the existing sandbox of the spec with the spec's ports and
/// environment variables, and the git identity.
pub(super) fn existing_start_request(
    spec: &SandboxSpec,
    workspace: &Path,
    git_identity: Option<GitIdentity>,
) -> StartSandboxRequest {
    StartSandboxRequest {
        env: Some(env_vars(spec.env.clone())),
        git_identity,
        ..start_request(&spec.name, workspace, Some(port_forwards(&spec.ports)))
    }
}

/// Sends the start request when the sandbox exists, waiting while it's starting. Returns the
/// daemon's response, or `None` when the sandbox doesn't exist.
pub(super) async fn start_if_exists(
    request: StartSandboxRequest,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<Option<SandboxStarted>> {
    let name = request.name.clone();
    let deadline = Instant::now() + STARTING_TIMEOUT;

    loop {
        let Some(status) = sandbox_status(client, &name).await? else {
            return Ok(None);
        };

        if let ControlFlow::Break(started) =
            start_existing_sandbox(status, request.clone(), client).await?
        {
            return Ok(Some(started));
        }

        if Instant::now() >= deadline {
            bail!("timed out waiting for sandbox {name} to start");
        }

        sleep(STARTING_POLL_INTERVAL).await;
    }
}

/// Sends the start request to an existing sandbox that is running or stopped, so a stopped one
/// starts and both get the request's ports. Returns `Continue` while the sandbox is still
/// starting, and fails when it can't be started now.
async fn start_existing_sandbox(
    status: SandboxStatus,
    request: StartSandboxRequest,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<ControlFlow<SandboxStarted>> {
    let name = request.name.clone();

    match status {
        SandboxStatus::Starting => Ok(ControlFlow::Continue(())),
        SandboxStatus::Running | SandboxStatus::Stopped | SandboxStatus::Crashed => {
            if status != SandboxStatus::Running {
                eprintln!("Starting sandbox {name}...");
            }

            let started = send_start_request(request, client).await?;

            Ok(ControlFlow::Break(started))
        }
        status @ (SandboxStatus::Stopping | SandboxStatus::Paused) => {
            bail!(
                "sandbox {name} is {}; try again once it has stopped",
                format_status(status).to_lowercase()
            )
        }
        // Only a listing reports an unknown status; GetSandbox fails instead.
        SandboxStatus::Unknown => {
            bail!("couldn't tell whether sandbox {name} is paused; try again")
        }
    }
}

/// Builds a start request from the spec, filling in the default image, resources, init, mise,
/// volumes and clipboard, with the git identity.
/// The workspace, the extra mounts and the mise dotfiles directory are mounted into the sandbox
/// when it's created. The workspace holds the spec file, so relative host paths resolve against
/// it. Fails when a mount host path or the mise config isn't an existing directory.
pub(super) fn build_start_request(
    spec: SandboxSpec,
    workspace: &Path,
    git_identity: Option<GitIdentity>,
) -> Result<StartSandboxRequest> {
    let resources = spec.resources.unwrap_or_default();
    let home = std::env::var_os(firebrick_utils::HOME_VAR);
    let mounts = resolve_mounts(spec.mounts.unwrap_or_default(), workspace, home.as_deref())?;
    let mise = spec.mise.unwrap_or_default();
    let mise_config = mise_config(&mise, workspace, home.as_deref())?;

    Ok(StartSandboxRequest {
        name: spec.name,
        image: spec
            .image
            .unwrap_or_else(|| firebrick_spec::DEFAULT_IMAGE.to_string()),
        resources: Some(SandboxResources {
            cpu: resources.cpu.into(),
            memory: resources.memory,
        }),
        volumes: Some(SandboxVolumes {
            docker: spec.volumes.docker,
        }),
        workspace: workspace.to_string_lossy().into_owned(),
        init: Some(spec.init.unwrap_or(true)),
        mise: Some(mise.is_enabled()),
        mise_config,
        network: spec.network.map(network_policy),
        ports: Some(port_forwards(&spec.ports)),
        mounts,
        clipboard: Some(spec.clipboard.unwrap_or(false)),
        env: Some(env_vars(spec.env)),
        git_identity,
    })
}

/// Turns the environment variables of a spec into their API message.
fn env_vars(env: BTreeMap<String, String>) -> EnvVars {
    EnvVars {
        vars: env.into_iter().collect(),
    }
}

/// Turns the network section of a spec into its API message.
pub(crate) fn network_policy(network: NetworkSpec) -> NetworkPolicy {
    let rules = |rules: Vec<NetworkRule>| rules.iter().map(ToString::to_string).collect();

    NetworkPolicy {
        enabled: network.enabled,
        enforce: network.enforce,
        allow: rules(network.allow),
        deny: rules(network.deny),
    }
}

const STARTING_TIMEOUT: Duration = Duration::from_secs(120);
const STARTING_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Makes sure the sandbox exists and is running, creating or starting it when needed, and
/// sends the spec's ports, also to a running sandbox. Warns about forwards the daemon couldn't
/// open and returns its response. Both requests carry the host's git identity for the
/// workspace. Fails when an existing sandbox mounts another directory than the workspace. A new
/// sandbox only gets mounts and network settings the user approved; `assume_yes` approves them
/// without asking.
pub(crate) async fn ensure_running(
    spec: SandboxSpec,
    workspace: &Path,
    assume_yes: bool,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<SandboxStarted> {
    check_workspace_owner(&spec.name, workspace, client).await?;

    let git_identity = git::host_identity(workspace).await;
    let request = existing_start_request(&spec, workspace, git_identity.clone());
    let started = match start_if_exists(request, client).await? {
        Some(started) => started,
        None => {
            let network = spec.network.clone().unwrap_or_default();
            let request = build_start_request(spec, workspace, git_identity)?;
            approve_new_sandbox(&request, network, workspace, assume_yes).await?;

            eprintln!("Creating sandbox {}...", request.name);
            send_start_request(request, client).await?
        }
    };

    warn_failed_forwards(&started);
    warn_env_pending_restart(&started);

    Ok(started)
}

/// What a message of a stream with image pull progress holds.
pub(super) enum Streamed<T> {
    /// The progress of the image pull.
    Progress(ImagePullProgress),
    /// The progress of installing the workspace's mise tools, which follows the pull.
    Mise(MiseInstallProgress),
    /// The result that ends the stream.
    Done(T),
    /// Nothing the client knows.
    Empty,
}

/// Reads the stream, showing the progress of the image pull and the mise install, until the
/// message `classify` finds the result in. Returns the result, or `None` when the stream ends without one.
pub(super) async fn follow_pull<R, T>(
    mut stream: Streaming<R>,
    classify: impl Fn(R) -> Streamed<T>,
) -> Result<Option<T>, tonic::Status> {
    let mut pull = PullProgressView::stderr();
    let mut mise = MiseProgressView::stderr();

    while let Some(response) = stream.message().await? {
        match classify(response) {
            Streamed::Progress(progress) => pull.update(&progress),
            Streamed::Mise(progress) => {
                pull.finish();
                mise.update(&progress);
            }
            Streamed::Done(result) => {
                pull.finish();
                mise.finish();
                return Ok(Some(result));
            }
            Streamed::Empty => {}
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Mount;
    use crate::spec_file::read_spec_file;
    use crate::test_support::symlinked_spec;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn sandbox(hostname: &str, workspace_path: &str) -> GetSandboxResponse {
        GetSandboxResponse {
            name: "project".to_string(),
            hostname: hostname.to_string(),
            workspace_path: workspace_path.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn connect_instructions_include_editor_commands() {
        assert_eq!(
            connect_instructions(&sandbox("project.fbk", "/workspaces/project")),
            "Connect with: ssh project.fbk\n\
             Open in VS Code: code --folder-uri \
             vscode-remote://ssh-remote+project.fbk/workspaces/project\n\
             Open in Zed: zed ssh://project.fbk/workspaces/project\n"
        );
    }

    #[test]
    fn connect_instructions_skip_editors_without_workspace_path() {
        assert_eq!(
            connect_instructions(&sandbox("project.fbk", "")),
            "Connect with: ssh project.fbk\n"
        );
    }

    #[test]
    fn connect_instructions_are_empty_without_hostname() {
        assert_eq!(
            connect_instructions(&sandbox("", "/workspaces/project")),
            ""
        );
    }

    #[test]
    fn start_request_carries_workspace() {
        let spec = firebrick_spec::default_spec("dev".to_string());

        let request = build_start_request(spec, Path::new("/home/user/project"), None).unwrap();

        assert_eq!(request.workspace, "/home/user/project");
    }

    fn identity() -> GitIdentity {
        GitIdentity {
            name: Some("Ada Lovelace".to_string()),
            email: Some("ada@example.com".to_string()),
        }
    }

    #[test]
    fn start_request_carries_the_git_identity() {
        let spec = firebrick_spec::default_spec("dev".to_string());

        let request =
            build_start_request(spec, Path::new("/home/user/project"), Some(identity())).unwrap();

        assert_eq!(request.git_identity, Some(identity()));
    }

    #[test]
    fn existing_start_request_carries_the_git_identity_ports_and_env() {
        let mut spec = firebrick_spec::default_spec("dev".to_string());
        spec.env = BTreeMap::from([("A".to_string(), "1".to_string())]);

        let request =
            existing_start_request(&spec, Path::new("/home/user/project"), Some(identity()));

        assert_eq!(request.git_identity, Some(identity()));
        assert_eq!(request.ports, Some(port_forwards(&spec.ports)));
        assert_eq!(request.env, Some(env_vars(spec.env)));
    }

    #[test]
    fn named_start_request_carries_no_git_identity() {
        let request = start_request("dev", Path::new(""), None);

        assert_eq!(request.git_identity, None);
    }

    #[test]
    fn start_request_carries_image_and_resources_from_spec() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(SPEC_FILE_NAME),
            "name: dev\nimage: alpine:3.22\ninit: false\nmise:\n  enabled: false\nresources:\n  cpu: 4\n  memory: 8Gi\nvolumes:\n  docker: 40 GiB\n",
        )
        .unwrap();

        let request = build_start_request(
            read_spec_file(dir.path()).unwrap().unwrap(),
            dir.path(),
            None,
        )
        .unwrap();
        let resources = request.resources.unwrap();

        assert_eq!(request.image, "alpine:3.22");
        assert_eq!(request.init, Some(false));
        assert_eq!(request.mise, Some(false));
        assert_eq!((resources.cpu, resources.memory.as_str()), (4, "8Gi"));
        assert_eq!(request.volumes.unwrap().docker, "40 GiB");
    }

    #[test]
    fn start_request_carries_network_from_spec() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(SPEC_FILE_NAME),
            "name: dev\nnetwork:\n  enabled: false\n  enforce: true\n  allow: [github.com, \"*.example.com\", 10.0.0.1, 10.0.0.0/8]\n  deny: [gist.github.com]\n",
        )
        .unwrap();

        let request = build_start_request(
            read_spec_file(dir.path()).unwrap().unwrap(),
            dir.path(),
            None,
        )
        .unwrap();

        assert_eq!(
            request.network,
            Some(NetworkPolicy {
                enforce: true,
                allow: vec![
                    "github.com".to_string(),
                    "*.example.com".to_string(),
                    "10.0.0.1".to_string(),
                    "10.0.0.0/8".to_string(),
                ],
                deny: vec!["gist.github.com".to_string()],
                enabled: Some(false),
            })
        );
    }

    #[test]
    fn start_request_carries_ports_from_spec() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(SPEC_FILE_NAME),
            "name: dev\nports: [3000, \"8080:5173\"]\n",
        )
        .unwrap();

        let request = build_start_request(
            read_spec_file(dir.path()).unwrap().unwrap(),
            dir.path(),
            None,
        )
        .unwrap();

        assert_eq!(
            request.ports,
            Some(PortForwards {
                ports: vec![
                    PortForward {
                        host: 3000,
                        guest: 3000
                    },
                    PortForward {
                        host: 8080,
                        guest: 5173
                    },
                ]
            })
        );
    }

    #[test]
    fn start_request_carries_env_from_spec() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(SPEC_FILE_NAME),
            "name: dev\nenv:\n  GREETING: \"$HOME\"\n  PORT: 3000\n",
        )
        .unwrap();

        let request = build_start_request(
            read_spec_file(dir.path()).unwrap().unwrap(),
            dir.path(),
            None,
        )
        .unwrap();

        assert_eq!(
            request.env,
            Some(EnvVars {
                vars: [("GREETING", "$HOME"), ("PORT", "3000")]
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .into()
            })
        );
    }

    #[test]
    fn start_request_without_env_in_spec_carries_no_variables() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), "name: dev\n").unwrap();

        let request = build_start_request(
            read_spec_file(dir.path()).unwrap().unwrap(),
            dir.path(),
            None,
        )
        .unwrap();

        assert_eq!(request.env, Some(EnvVars::default()));
    }

    #[test]
    fn forward_lines_name_host_and_sandbox_ports() {
        let started = SandboxStarted {
            forwards: vec![PortForward {
                host: 8080,
                guest: 5173,
            }],
            ..Default::default()
        };

        assert_eq!(
            forward_lines(&started),
            "Forwarding localhost:8080 -> sandbox port 5173\n"
        );
    }

    #[test]
    fn start_request_fills_in_defaults() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), "name: dev\n").unwrap();

        let request = build_start_request(
            read_spec_file(dir.path()).unwrap().unwrap(),
            dir.path(),
            None,
        )
        .unwrap();
        let resources = request.resources.unwrap();
        let defaults = firebrick_spec::SandboxResourcesSpec::default();

        assert_eq!(request.image, firebrick_spec::DEFAULT_IMAGE);
        assert_eq!(request.init, Some(true));
        assert_eq!(request.mise, Some(true));
        assert_eq!(request.network, None);
        assert!(request.mounts.is_empty());
        assert_eq!(request.ports, Some(PortForwards { ports: vec![] }));
        assert_eq!(request.clipboard, Some(false));
        assert_eq!(resources.cpu, u32::from(defaults.cpu));
        assert_eq!(resources.memory, defaults.memory);
        assert_eq!(
            request.volumes.unwrap().docker,
            firebrick_spec::VolumesSpec::default().docker
        );
    }

    #[test]
    fn clipboard_from_the_spec_is_sent_and_needs_approval() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(SPEC_FILE_NAME),
            "name: dev\nclipboard: true\n",
        )
        .unwrap();

        let request = build_start_request(
            read_spec_file(dir.path()).unwrap().unwrap(),
            dir.path(),
            None,
        )
        .unwrap();
        let settings = new_sandbox_settings(&request, NetworkSpec::default());

        assert_eq!(request.clipboard, Some(true));
        assert_eq!(settings.clipboard, Clipboard::Both);
    }

    #[test]
    fn start_request_carries_mounts_from_spec() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(dir.path().join("shared-lib")).unwrap();
        fs::write(
            project.join(SPEC_FILE_NAME),
            "name: dev\nmounts:\n  - host: ../shared-lib\n    guest: /workspaces/shared-lib\n    readonly: true\n",
        )
        .unwrap();

        let request =
            build_start_request(read_spec_file(&project).unwrap().unwrap(), &project, None)
                .unwrap();

        assert_eq!(
            request.mounts,
            [Mount {
                host: dunce::canonicalize(dir.path().join("shared-lib"))
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                guest: "/workspaces/shared-lib".to_string(),
                readonly: true,
            }]
        );
    }

    #[test]
    fn new_sandbox_settings_show_the_target_of_a_symlinked_mount() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        let target = dir.path().join("target");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&target).unwrap();
        if !symlinked_spec::symlink_dir(&target, &project.join("lib")) {
            return;
        }
        fs::write(
            project.join(SPEC_FILE_NAME),
            "name: dev\nmounts:\n  - host: lib\n    guest: /mnt/lib\n",
        )
        .unwrap();
        let request =
            build_start_request(read_spec_file(&project).unwrap().unwrap(), &project, None)
                .unwrap();

        let settings = new_sandbox_settings(&request, NetworkSpec::default());

        assert_eq!(
            settings.mounts,
            [MountSpec {
                host: dunce::canonicalize(&target)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                guest: "/mnt/lib".to_string(),
                readonly: false,
            }]
        );
    }

    /// Writes a project with a spec whose `mise` section is `mise`, next to a `dotfiles`
    /// directory, with a `mise.toml` when `with_config` is set. Returns the temp dir, the
    /// project and the canonical dotfiles directory.
    fn project_with_dotfiles(mise: &str, with_config: bool) -> (TempDir, PathBuf, String) {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dotfiles).unwrap();
        if with_config {
            fs::write(dotfiles.join("mise.toml"), "[dotfiles]\n").unwrap();
        }
        fs::write(
            project.join(SPEC_FILE_NAME),
            format!("name: dev\nmise:\n{mise}"),
        )
        .unwrap();
        let canonical = dunce::canonicalize(&dotfiles)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        (dir, project, canonical)
    }

    fn start_request_of(project: &Path) -> Result<StartSandboxRequest> {
        build_start_request(read_spec_file(project).unwrap().unwrap(), project, None)
    }

    #[test]
    fn start_request_carries_the_mise_config_and_approval_shows_its_mount() {
        let (_dir, project, dotfiles) = project_with_dotfiles("  config: ../dotfiles\n", true);

        let request = start_request_of(&project).unwrap();
        let settings = new_sandbox_settings(&request, NetworkSpec::default());

        assert_eq!(request.mise, Some(true));
        assert_eq!(request.mise_config, dotfiles);
        assert_eq!(
            settings.mounts,
            [MountSpec {
                host: dotfiles,
                guest: "/opt/firebrick/dotfiles".to_string(),
                readonly: true,
            }]
        );
    }

    #[test]
    fn start_request_without_a_mise_toml_carries_no_mise_config() {
        let (_dir, project, _) = project_with_dotfiles("  config: ../dotfiles\n", false);

        let request = start_request_of(&project).unwrap();

        assert_eq!(request.mise_config, "");
        assert!(
            new_sandbox_settings(&request, NetworkSpec::default())
                .mounts
                .is_empty()
        );
    }

    #[test]
    fn start_request_with_mise_disabled_carries_no_mise_config() {
        let (_dir, project, _) =
            project_with_dotfiles("  enabled: false\n  config: ../dotfiles\n", true);

        let request = start_request_of(&project).unwrap();

        assert_eq!(request.mise, Some(false));
        assert_eq!(request.mise_config, "");
    }

    #[test]
    fn start_request_fails_for_a_missing_mise_config() {
        let (_dir, project, _) = project_with_dotfiles("  config: ../missing\n", true);

        let err = start_request_of(&project).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                "can't mount {}: No such file or directory",
                project.join("../missing").display()
            )
        );
    }

    #[test]
    fn start_request_fails_for_missing_mount() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(SPEC_FILE_NAME),
            "name: dev\nmounts:\n  - host: missing\n    guest: /data\n",
        )
        .unwrap();

        let result = build_start_request(
            read_spec_file(dir.path()).unwrap().unwrap(),
            dir.path(),
            None,
        );

        assert!(result.is_err());
    }
}
