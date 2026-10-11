use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use firebrick_spec::{MountSpec, NetworkRule, NetworkSpec, PortMapping, SandboxSpec};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tonic::Code;
use tonic::transport::Channel;

use crate::api::{
    GetSandboxRequest, GetSandboxResponse, ListSandboxesRequest, ListSandboxesResponse, Mount,
    NetworkPolicy, PortForward, PortForwards, RemoveSandboxRequest, SandboxResources,
    SandboxStarted, SandboxStatus, SandboxSummary, SandboxVolumes, StartSandboxRequest,
    StopSandboxRequest, sandbox_management_service_client::SandboxManagementServiceClient,
    start_sandbox_response,
};
use crate::progress::PullProgressView;
use crate::table;

/// Name of the spec file `fbk` looks for in the working directory.
pub const SPEC_FILE_NAME: &str = ".firebrick.yml";

/// Starts the named sandbox, or the sandbox for the working directory without a name. Does
/// nothing when it's already running, and waits for it when it's starting. Only the sandbox for
/// the working directory is created when it doesn't exist, because creating one needs its spec.
pub async fn start_sandbox(
    name: Option<String>,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let (name, started) = match name {
        Some(name) => start_named_sandbox(name, client).await?,
        None => start_working_dir_sandbox(working_dir, client).await?,
    };

    let sandbox = client
        .get_sandbox(GetSandboxRequest { name })
        .await?
        .into_inner();

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
fn warn_failed_forwards(started: &SandboxStarted) {
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
fn connect_instructions(sandbox: &GetSandboxResponse) -> String {
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
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<(String, SandboxStarted)> {
    let spec = resolve_spec(working_dir, client).await?;
    let name = spec.name.clone();

    let started = ensure_running(spec, working_dir, client).await?;

    Ok((name, started))
}

const STARTING_TIMEOUT: Duration = Duration::from_secs(120);
const STARTING_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Makes sure the sandbox exists and is running, creating or starting it when needed, and
/// sends the spec's ports, also to a running sandbox. Warns about forwards the daemon couldn't
/// open and returns its response. Fails when an existing sandbox mounts another directory than
/// the workspace.
pub(crate) async fn ensure_running(
    spec: SandboxSpec,
    workspace: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<SandboxStarted> {
    check_workspace_owner(&spec.name, workspace, client).await?;

    let ports = port_forwards(&spec.ports);
    let request = start_request(&spec.name, workspace, Some(ports));
    let started = match start_if_exists(request, client).await? {
        Some(started) => started,
        None => {
            let request = build_start_request(spec, workspace)?;

            eprintln!("Creating sandbox {}...", request.name);
            send_start_request(request, client).await?
        }
    };

    warn_failed_forwards(&started);

    Ok(started)
}

/// Sends the start request and shows the progress of the image pull while the daemon creates
/// the sandbox. Returns the message that ends the stream.
async fn send_start_request(
    request: StartSandboxRequest,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<SandboxStarted> {
    let mut stream = client.start_sandbox(request).await?.into_inner();
    let mut pull = PullProgressView::stderr();

    while let Some(response) = stream.message().await? {
        match response.message {
            Some(start_sandbox_response::Message::PullProgress(progress)) => {
                pull.update(&progress);
            }
            Some(start_sandbox_response::Message::Started(started)) => {
                pull.finish();
                return Ok(started);
            }
            None => {}
        }
    }

    bail!("the daemon ended the start without starting the sandbox")
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
        Err(status) => return Err(status.into()),
    };

    if host_path.is_empty() {
        return Ok(());
    }

    // microsandbox stores the mounted host path canonicalized, so compare it canonicalized.
    let working_dir = std::fs::canonicalize(workspace).with_context(|| {
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
        "sandbox {name} belongs to {host_path}; add a {SPEC_FILE_NAME} with its own name to give \
         this directory a separate sandbox"
    )
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

/// Sends the start request when the sandbox exists, waiting while it's starting. Returns the
/// daemon's response, or `None` when the sandbox doesn't exist.
async fn start_if_exists(
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
    }
}

/// Builds a start request from the spec, filling in the default image, resources, init, mise
/// and volumes.
/// The workspace and the extra mounts are mounted into the sandbox when it's created. The
/// workspace holds the spec file, so relative mount host paths resolve against it. Fails when a
/// mount host path isn't an existing directory.
fn build_start_request(spec: SandboxSpec, workspace: &Path) -> Result<StartSandboxRequest> {
    let resources = spec.resources.unwrap_or_default();
    let mounts = resolve_mounts(
        spec.mounts.unwrap_or_default(),
        workspace,
        std::env::var_os("HOME"),
    )?;

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
        mise: Some(spec.mise.unwrap_or(true)),
        network: spec.network.map(network_policy),
        ports: Some(port_forwards(&spec.ports)),
        mounts,
    })
}

/// Turns the mounts of a spec into their API messages, with canonical absolute host paths.
fn resolve_mounts(
    mounts: Vec<MountSpec>,
    spec_dir: &Path,
    home: Option<OsString>,
) -> Result<Vec<Mount>> {
    mounts
        .into_iter()
        .map(|mount| {
            let host = resolve_host_path(&mount.host, spec_dir, home.as_deref())?;

            Ok(Mount {
                host: host.to_string_lossy().into_owned(),
                guest: mount.guest,
                readonly: mount.readonly,
            })
        })
        .collect()
}

/// Resolves a mount host path to the canonical path of an existing directory. A leading `~`
/// expands to the home directory, `~user` to the home directory of that user, and a relative
/// path resolves against the spec file's directory.
fn resolve_host_path(
    host: &str,
    spec_dir: &Path,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    let path = match host.strip_prefix('~') {
        Some(rest) => expand_tilde(host, rest, home)?,
        None => spec_dir.join(host),
    };

    let canonical = std::fs::canonicalize(&path).map_err(|err| mount_error(&path, &err))?;

    if !canonical.is_dir() {
        bail!("can't mount {}: not a directory", canonical.display());
    }

    Ok(canonical)
}

/// Expands `~` or `~user` at the start of a host path, the way a shell does. `rest` is the
/// host path without its `~`.
fn expand_tilde(host: &str, rest: &str, home: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    let (user, tail) = rest.split_once('/').unwrap_or((rest, ""));

    let home = if user.is_empty() {
        PathBuf::from(home.with_context(|| format!("can't expand ~ in {host}: $HOME isn't set"))?)
    } else {
        user_home(host, user)?
    };

    Ok(home.join(tail))
}

/// Looks up the home directory of the user in the user database.
fn user_home(host: &str, user: &str) -> Result<PathBuf> {
    let entry = nix::unistd::User::from_name(user)
        .with_context(|| format!("can't expand ~{user} in {host}"))?
        .ok_or_else(|| anyhow!("can't expand {host}: user {user} doesn't exist"))?;

    Ok(entry.dir)
}

/// Describes why the host path can't be mounted, without the OS error code.
fn mount_error(path: &Path, err: &io::Error) -> anyhow::Error {
    let reason = match err.kind() {
        io::ErrorKind::NotFound => "No such file or directory".to_string(),
        _ => err.to_string(),
    };

    anyhow!("can't mount {}: {reason}", path.display())
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

/// Returns the explicit sandbox name, or the name of the sandbox for the working directory.
/// The spec file is only read, and the sandbox's workspace only checked, without an explicit
/// name.
async fn sandbox_name(
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
fn describe_status(name: &str, status: tonic::Status) -> anyhow::Error {
    if status.code() == Code::NotFound {
        return anyhow!("sandbox {name} doesn't exist");
    }

    status.into()
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

/// Output format for the list of sandboxes.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// A human-readable table.
    #[default]
    Table,
    /// A JSON array of sandboxes.
    Json,
}

/// Prints all sandboxes with their status in the given format.
pub async fn list_sandboxes(
    client: &mut SandboxManagementServiceClient<Channel>,
    format: OutputFormat,
) -> Result<()> {
    let response = client.list_sandboxes(ListSandboxesRequest {}).await?;
    let response_data: ListSandboxesResponse = response.into_inner();

    let output = match format {
        OutputFormat::Table => render_sandbox_table(&response_data.sandboxes)?,
        OutputFormat::Json => render_sandbox_json(&response_data.sandboxes)?,
    };

    print!("{output}");

    Ok(())
}

const TABLE_HEADER: [&str; 3] = ["NAME", "STATUS", "HOSTNAME"];

/// Renders the sandboxes as a table with their name, status and host name.
fn render_sandbox_table(sandboxes: &[SandboxSummary]) -> Result<String> {
    let rows: Vec<[String; 3]> = sandboxes
        .iter()
        .map(|item| {
            [
                item.name.clone(),
                format_status(item.status()),
                item.hostname.clone(),
            ]
        })
        .collect();

    table::render(TABLE_HEADER, &rows)
}

/// A sandbox as printed in the JSON output.
#[derive(Serialize)]
struct SandboxListing<'a> {
    name: &'a str,
    status: String,
    hostname: &'a str,
}

/// Renders the sandboxes as a pretty-printed JSON array.
fn render_sandbox_json(sandboxes: &[SandboxSummary]) -> Result<String> {
    let listings: Vec<SandboxListing> = sandboxes
        .iter()
        .map(|item| SandboxListing {
            name: &item.name,
            status: format_status(item.status()).to_lowercase(),
            hostname: &item.hostname,
        })
        .collect();

    Ok(format!("{}\n", serde_json::to_string_pretty(&listings)?))
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

/// Returns the legacy name when a sandbox with it exists, and the hashed name otherwise.
fn default_name(working_dir: &Path, legacy_exists: bool) -> String {
    match legacy_name(working_dir) {
        Some(name) if legacy_exists => name,
        _ => hashed_name(working_dir),
    }
}

/// Loads the spec file from the working directory, or returns `None` when there isn't one.
fn read_spec_file(working_dir: &Path) -> Result<Option<SandboxSpec>> {
    let spec_path = working_dir.join(SPEC_FILE_NAME);

    if !spec_path.is_file() {
        return Ok(None);
    }

    Ok(Some(firebrick_spec::from_file(&spec_path)?))
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
fn legacy_name(working_dir: &Path) -> Option<String> {
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

/// Returns a human-readable label for a sandbox status.
fn format_status(status: SandboxStatus) -> String {
    let status_text = match status {
        SandboxStatus::Running => "Running",
        SandboxStatus::Stopped => "Stopped",
        SandboxStatus::Starting => "Starting",
        SandboxStatus::Paused => "Paused",
        SandboxStatus::Stopping => "Stopping",
        SandboxStatus::Crashed => "Crashed",
    };

    status_text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon;
    use std::fs;
    use tempfile::TempDir;

    fn summary(name: &str, status: SandboxStatus, hostname: &str) -> SandboxSummary {
        SandboxSummary {
            name: name.to_string(),
            status: status.into(),
            hostname: hostname.to_string(),
        }
    }

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
    fn table_aligns_sandboxes_under_header() {
        let sandboxes = [
            summary("dev", SandboxStatus::Running, "dev.fbk"),
            summary("long_project_name", SandboxStatus::Stopped, ""),
        ];

        let table = render_sandbox_table(&sandboxes).unwrap();

        assert_eq!(
            table,
            "┌───────────────────┬─────────┬──────────┐\n\
             │ NAME              │ STATUS  │ HOSTNAME │\n\
             ├───────────────────┼─────────┼──────────┤\n\
             │ dev               │ Running │ dev.fbk  │\n\
             │ long_project_name │ Stopped │          │\n\
             └───────────────────┴─────────┴──────────┘\n"
        );
    }

    #[test]
    fn table_aligns_wide_characters() {
        let sandboxes = [summary("名前", SandboxStatus::Running, "")];

        let table = render_sandbox_table(&sandboxes).unwrap();
        let widths: Vec<usize> = table
            .lines()
            .map(|line| ratatui::text::Line::from(line).width())
            .collect();

        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]), "{table}");
    }

    #[test]
    fn empty_table_shows_header() {
        let table = render_sandbox_table(&[]).unwrap();

        assert_eq!(
            table,
            "┌──────┬────────┬──────────┐\n\
             │ NAME │ STATUS │ HOSTNAME │\n\
             ├──────┼────────┼──────────┤\n\
             └──────┴────────┴──────────┘\n"
        );
    }

    #[test]
    fn json_lists_sandboxes() {
        let sandboxes = [summary("dev", SandboxStatus::Running, "dev.fbk")];

        let json = render_sandbox_json(&sandboxes).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            value,
            serde_json::json!([{ "name": "dev", "status": "running", "hostname": "dev.fbk" }])
        );
    }

    #[test]
    fn empty_json_is_empty_array() {
        assert_eq!(render_sandbox_json(&[]).unwrap(), "[]\n");
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

    /// Returns a working directory whose spec file names the sandbox `name`.
    fn workspace_named(name: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SPEC_FILE_NAME), format!("name: {name}\n")).unwrap();
        dir
    }

    const OTHER_WORKSPACE: &str = "/home/user/other-project";

    #[tokio::test]
    async fn without_name_refuses_sandbox_of_other_workspace() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        let error = sandbox_name(None, dir.path(), &mut client)
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("sandbox dev belongs to /home/user/other-project;"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn without_name_accepts_sandbox_of_working_directory() {
        let dir = workspace_named("dev");
        let canonical = fs::canonicalize(dir.path()).unwrap();
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

    #[tokio::test]
    async fn remove_without_name_refuses_sandbox_of_other_workspace() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        let error = remove_sandbox(None, true, dir.path(), &mut client)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("belongs to"), "{error}");
    }

    #[tokio::test]
    async fn stop_without_name_refuses_sandbox_of_other_workspace() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        let error = stop_sandbox(None, dir.path(), &mut client)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("belongs to"), "{error}");
    }

    #[tokio::test]
    async fn remove_with_explicit_name_skips_owner_check() {
        let dir = workspace_named("dev");
        let mut client = fake_daemon::serve("dev", OTHER_WORKSPACE).await;

        let error = remove_sandbox(Some("dev".to_string()), true, dir.path(), &mut client)
            .await
            .unwrap_err();

        assert!(error.to_string().contains(fake_daemon::REFUSED), "{error}");
    }

    #[test]
    fn not_found_status_names_missing_sandbox() {
        let error = describe_status("dev", tonic::Status::not_found("couldn't find it"));

        assert_eq!(error.to_string(), "sandbox dev doesn't exist");
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

    #[test]
    fn other_status_is_kept() {
        let error = describe_status("dev", tonic::Status::internal("failed to stop sandbox"));

        assert!(
            error.to_string().contains("failed to stop sandbox"),
            "{error}"
        );
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
    fn root_path_has_no_legacy_name() {
        assert_eq!(legacy_name(Path::new("/")), None);
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
            "sandbox firebrick-d9f287 belongs to /home/user/my-project; add a .firebrick.yml with \
             its own name to give this directory a separate sandbox"
        );
    }

    #[test]
    fn sandbox_without_workspace_mount_passes() {
        let result = ensure_same_workspace("dev", "", Path::new("/home/user/other-project"));

        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn start_request_carries_workspace() {
        let spec = firebrick_spec::default_spec("dev".to_string());

        let request = build_start_request(spec, Path::new("/home/user/project")).unwrap();

        assert_eq!(request.workspace, "/home/user/project");
    }

    #[test]
    fn start_request_carries_image_and_resources_from_spec() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(SPEC_FILE_NAME),
            "name: dev\nimage: alpine:3.22\ninit: false\nmise: false\nresources:\n  cpu: 4\n  memory: 8Gi\nvolumes:\n  docker: 40 GiB\n",
        )
        .unwrap();

        let request =
            build_start_request(read_spec_file(dir.path()).unwrap().unwrap(), dir.path()).unwrap();
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

        let request =
            build_start_request(read_spec_file(dir.path()).unwrap().unwrap(), dir.path()).unwrap();

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

        let request =
            build_start_request(read_spec_file(dir.path()).unwrap().unwrap(), dir.path()).unwrap();

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

        let request =
            build_start_request(read_spec_file(dir.path()).unwrap().unwrap(), dir.path()).unwrap();
        let resources = request.resources.unwrap();
        let defaults = firebrick_spec::SandboxResourcesSpec::default();

        assert_eq!(request.image, firebrick_spec::DEFAULT_IMAGE);
        assert_eq!(request.init, Some(true));
        assert_eq!(request.mise, Some(true));
        assert_eq!(request.network, None);
        assert!(request.mounts.is_empty());
        assert_eq!(request.ports, Some(PortForwards { ports: vec![] }));
        assert_eq!(resources.cpu, u32::from(defaults.cpu));
        assert_eq!(resources.memory, defaults.memory);
        assert_eq!(
            request.volumes.unwrap().docker,
            firebrick_spec::VolumesSpec::default().docker
        );
    }

    #[test]
    fn start_request_carries_mounts_from_spec() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(project.join("../shared-lib")).unwrap();
        fs::write(
            project.join(SPEC_FILE_NAME),
            "name: dev\nmounts:\n  - host: ../shared-lib\n    guest: /workspaces/shared-lib\n    readonly: true\n",
        )
        .unwrap();

        let request =
            build_start_request(read_spec_file(&project).unwrap().unwrap(), &project).unwrap();

        assert_eq!(
            request.mounts,
            [Mount {
                host: fs::canonicalize(dir.path().join("shared-lib"))
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                guest: "/workspaces/shared-lib".to_string(),
                readonly: true,
            }]
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

        let result = build_start_request(read_spec_file(dir.path()).unwrap().unwrap(), dir.path());

        assert!(result.is_err());
    }

    #[test]
    fn host_path_resolves_absolute_path() {
        let dir = TempDir::new().unwrap();
        let host = dir.path().to_str().unwrap();

        let resolved = resolve_host_path(host, Path::new("/elsewhere"), None).unwrap();

        assert_eq!(resolved, fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn host_path_resolves_relative_to_spec_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("project")).unwrap();
        fs::create_dir_all(dir.path().join("shared")).unwrap();

        let resolved = resolve_host_path("../shared", &dir.path().join("project"), None).unwrap();

        assert_eq!(
            resolved,
            fs::canonicalize(dir.path().join("shared")).unwrap()
        );
    }

    #[test]
    fn host_path_expands_home() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("datasets")).unwrap();
        let home = Some(dir.path().as_os_str());

        let resolved = resolve_host_path("~/datasets", Path::new("/elsewhere"), home).unwrap();
        let home_only = resolve_host_path("~", Path::new("/elsewhere"), home).unwrap();

        assert_eq!(
            resolved,
            fs::canonicalize(dir.path().join("datasets")).unwrap()
        );
        assert_eq!(home_only, fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn host_path_expands_other_users_home() {
        // root exists on every Linux and macOS host; its home differs between them.
        let root_home = nix::unistd::User::from_name("root").unwrap().unwrap().dir;

        let resolved = resolve_host_path("~root", Path::new("/elsewhere"), None).unwrap();

        assert_eq!(resolved, fs::canonicalize(root_home).unwrap());
    }

    #[test]
    fn host_path_with_unknown_user_is_refused() {
        let error =
            resolve_host_path("~fbk-no-such-user/data", Path::new("/elsewhere"), None).unwrap_err();

        assert_eq!(
            error.to_string(),
            "can't expand ~fbk-no-such-user/data: user fbk-no-such-user doesn't exist"
        );
    }

    #[test]
    fn host_path_needs_home_for_tilde() {
        let error = resolve_host_path("~/datasets", Path::new("/elsewhere"), None).unwrap_err();

        assert!(error.to_string().contains("$HOME isn't set"), "{error}");
    }

    #[test]
    fn missing_host_path_is_refused() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing");

        let error = resolve_host_path("missing", dir.path(), None).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "can't mount {}: No such file or directory",
                missing.display()
            )
        );
    }

    #[test]
    fn host_path_that_is_a_file_is_refused() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file"), "").unwrap();
        let file = fs::canonicalize(dir.path().join("file")).unwrap();

        let error = resolve_host_path("file", dir.path(), None).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("can't mount {}: not a directory", file.display())
        );
    }
}
