//! Port forwards of the working directory's sandbox: `fbk port forward` and `fbk port rm` change
//! them in the daemon right away and record them in `.firebrick.yml`, and `fbk port ls` shows
//! them next to the TCP ports listening in the sandbox.

use anyhow::{Result, anyhow, bail};
use firebrick_spec::PortMapping;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use tonic::Code;
use tonic::transport::Channel;

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use crate::api::{
    ForwardPortRequest, GuestListener, ListPortsRequest, ListPortsResponse, PortForward,
    RemovePortRequest,
};
use crate::sandbox::{self, OutputFormat};
use crate::spec_file::{self, SPEC_FILE_NAME};
use crate::ui::table;

/// Forwards the host port to the guest port of the working directory's sandbox, replacing the
/// forward of the same host port, and records it in the spec file. The spec file only changes
/// once the daemon accepted the forward.
pub async fn forward(
    port: PortMapping,
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let spec = SpecFile::read(working_dir, client).await?;
    let edited = firebrick_spec::add_port(&spec.text, port).map_err(spec_file::spec_error)?;

    let running = match sandbox::sandbox_status(client, &spec.name).await? {
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
    sandbox::warn_if_outdated(client, &spec.name).await;

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
    let edited = firebrick_spec::remove_port(&spec.text, host).map_err(spec_file::spec_error)?;

    let running = match sandbox::sandbox_status(client, &spec.name).await? {
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
    sandbox::warn_if_outdated(client, &spec.name).await;

    Ok(())
}

/// Prints the forwards of the working directory's sandbox and the TCP ports listening in it, in
/// the format. Only reads: it changes neither the spec file nor the sandbox. Notes and warnings
/// go to stderr.
pub async fn list(
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
    format: OutputFormat,
) -> Result<()> {
    let listing = port_listing(working_dir, client, format).await?;

    if let Some(note) = &listing.note {
        eprintln!("{note}");
    }
    print!("{}", listing.output);

    Ok(())
}

/// What [`list`] prints: `output` on stdout and `note` on stderr.
#[derive(Debug)]
struct Listing {
    output: String,
    note: Option<String>,
}

/// Returns the text [`list`] prints.
async fn port_listing(
    working_dir: &Path,
    client: &mut SandboxManagementServiceClient<Channel>,
    format: OutputFormat,
) -> Result<Listing> {
    let name = sandbox::resolve_owned_spec(working_dir, client).await?.name;
    let request = ListPortsRequest { name: name.clone() };
    let response = client
        .list_ports(request)
        .await
        .map_err(|status| sandbox::describe_status(&name, status))?
        .into_inner();
    let rows = port_rows(&response);

    let output = match format {
        OutputFormat::Table if rows.is_empty() => {
            format!("Sandbox {name} has no forwards or listening ports.\n")
        }
        OutputFormat::Table => render_table(&rows)?,
        OutputFormat::Json => format!("{}\n", serde_json::to_string_pretty(&rows)?),
    };

    Ok(Listing {
        output,
        note: listing_note(&name, &response),
    })
}

/// Returns the note for a sandbox that doesn't run, or the warning when its listening ports
/// couldn't be read or are incomplete. The reason comes partly from the guest, so its control
/// characters are removed.
fn listing_note(name: &str, response: &ListPortsResponse) -> Option<String> {
    if !response.running {
        return Some(format!(
            "sandbox {name} isn't running; start it to see its listening ports"
        ));
    }

    if let Some(reason) = &response.listeners_error {
        let reason = firebrick_utils::strip_control(reason);
        return Some(format!(
            "warning: couldn't read the listening ports of sandbox {name}: {reason}"
        ));
    }

    response.listeners_truncated.then(|| {
        format!(
            "warning: sandbox {name} reported more than fbk reads; the list may miss listening \
             ports or their processes"
        )
    })
}

/// How a row's guest port is used.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
enum PortStatus {
    /// A forward exists and something listens.
    #[serde(rename = "forwarded")]
    Forwarded,
    /// Something listens and no forward exists.
    #[serde(rename = "listening")]
    Listening,
    /// A forward exists and nothing listens.
    #[serde(rename = "not listening")]
    NotListening,
    /// A forward of a sandbox that doesn't run.
    #[serde(rename = "stopped")]
    Stopped,
    /// A forward of a sandbox whose listening ports couldn't be read.
    #[serde(rename = "unknown")]
    Unknown,
}

impl fmt::Display for PortStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Forwarded => "forwarded",
            Self::Listening => "listening",
            Self::NotListening => "not listening",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        })
    }
}

/// The process that owns a listening port.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
struct ProcessInfo {
    name: String,
    pid: u32,
}

/// A row of `fbk port ls`: a forward, or a listening port without one.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
struct PortRow {
    host: Option<u16>,
    guest: u16,
    addresses: Vec<String>,
    process: Option<ProcessInfo>,
    status: PortStatus,
}

/// Merges the forwards with the listening ports: one row per forward, with what listens on its
/// guest port, and one per listening port that no forward reaches. Sorted by guest port, then
/// host port. Ports outside 1 to 65535 are skipped.
fn port_rows(response: &ListPortsResponse) -> Vec<PortRow> {
    let listeners: BTreeMap<u16, &GuestListener> = response
        .listeners
        .iter()
        .filter_map(|listener| Some((port_of(listener.port)?, listener)))
        .collect();
    let forwards: Vec<(u16, u16)> = response
        .forwards
        .iter()
        .filter_map(|forward| Some((port_of(forward.host)?, port_of(forward.guest)?)))
        .collect();

    let mut rows: Vec<PortRow> = forwards
        .iter()
        .map(|&(host, guest)| {
            let listener = listeners.get(&guest).copied();
            let status = forward_status(response, listener.is_some());
            port_row(Some(host), guest, listener, status)
        })
        .collect();
    rows.extend(
        listeners
            .iter()
            .filter(|(port, _)| !forwards.iter().any(|&(_, guest)| guest == **port))
            .map(|(&port, &listener)| port_row(None, port, Some(listener), PortStatus::Listening)),
    );
    rows.sort_by_key(|row| (row.guest, row.host));

    rows
}

/// The status of a forward whose guest port has a listener or not.
fn forward_status(response: &ListPortsResponse, listening: bool) -> PortStatus {
    match (response.running, &response.listeners_error, listening) {
        (false, _, _) => PortStatus::Stopped,
        (true, Some(_), _) => PortStatus::Unknown,
        (true, None, true) => PortStatus::Forwarded,
        (true, None, false) => PortStatus::NotListening,
    }
}

/// Returns the row of a guest port and what listens on it. The process name comes from the
/// guest, so its control characters are removed, and newlines and tabs become spaces so it fits
/// on one line.
fn port_row(
    host: Option<u16>,
    guest: u16,
    listener: Option<&GuestListener>,
    status: PortStatus,
) -> PortRow {
    PortRow {
        host,
        guest,
        addresses: listener.map(|l| l.addresses.clone()).unwrap_or_default(),
        process: listener.and_then(|l| {
            Some(ProcessInfo {
                name: firebrick_utils::strip_control(&l.process).replace(['\n', '\t'], " "),
                pid: l.pid?,
            })
        }),
        status,
    }
}

/// Returns the port number, or `None` when it's outside 1 to 65535.
fn port_of(port: u32) -> Option<u16> {
    u16::try_from(port).ok().filter(|&port| port != 0)
}

const PORTS_HEADER: [&str; 5] = ["HOST", "GUEST", "ADDRESS", "PROCESS", "STATUS"];

/// Renders the rows as a table, with `-` for empty cells.
fn render_table(rows: &[PortRow]) -> Result<String> {
    let rows: Vec<[String; 5]> = rows.iter().map(table_row).collect();

    table::render(PORTS_HEADER, &rows)
}

/// Returns the table cells of a row.
fn table_row(row: &PortRow) -> [String; 5] {
    let or_dash = |text: String| {
        if text.is_empty() {
            "-".to_string()
        } else {
            text
        }
    };

    [
        row.host
            .map_or_else(|| "-".to_string(), |host| host.to_string()),
        row.guest.to_string(),
        or_dash(row.addresses.join(", ")),
        row.process
            .as_ref()
            .map_or_else(|| "-".to_string(), |p| format!("{} ({})", p.name, p.pid)),
        row.status.to_string(),
    ]
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
        spec_file::migrate_spec(&path)?;

        let (name, text) = match firebrick_spec::read_text(&path).map_err(spec_file::spec_error)? {
            Some(text) => (
                firebrick_spec::from_str(&text)
                    .map_err(spec_file::spec_error)?
                    .name,
                text,
            ),
            None => {
                let name = sandbox::resolve_spec(working_dir, client).await?.name;
                let text = format!("name: {name}\n");
                (name, text)
            }
        };

        sandbox::check_workspace_owner(&name, working_dir, client).await?;

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
    use crate::test_support::fake_daemon::{self, OTHER_WORKSPACE};
    use crate::test_support::symlinked_spec;
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

    #[tokio::test]
    async fn remove_migrates_a_boolean_mise() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(SPEC_FILE_NAME),
            "name: dev\nmise: false\nports:\n  - 8080:80\n",
        )
        .unwrap();
        let mut client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        remove(8080, dir.path(), &mut client).await.unwrap();

        assert_eq!(
            spec_text(&dir),
            "name: dev\nmise:\n  enabled: false\nports: []\n"
        );
    }

    fn stored(host: u32, guest: u32) -> PortForward {
        PortForward { host, guest }
    }

    fn guest_listener(
        port: u32,
        addresses: &[&str],
        process: Option<(u32, &str)>,
    ) -> GuestListener {
        GuestListener {
            port,
            addresses: addresses.iter().map(ToString::to_string).collect(),
            pid: process.map(|(pid, _)| pid),
            process: process
                .map(|(_, name)| name.to_string())
                .unwrap_or_default(),
        }
    }

    /// A running sandbox that forwards 3000 and 8080 -> 8081, and listens on 3000 and 5173.
    fn running_ports() -> ListPortsResponse {
        ListPortsResponse {
            running: true,
            forwards: vec![stored(3000, 3000), stored(8080, 8081)],
            listeners: vec![
                guest_listener(3000, &["127.0.0.1"], Some((412, "node"))),
                guest_listener(5173, &["0.0.0.0", "::"], Some((530, "vite"))),
            ],
            listeners_error: None,
            listeners_truncated: false,
        }
    }

    /// Lists the ports of `dev`, which belongs to the workspace and answers with `ports`.
    async fn list_dev(ports: ListPortsResponse, format: OutputFormat) -> Listing {
        let dir = workspace();
        let path = dunce::canonicalize(dir.path()).unwrap();
        let mut client = fake_daemon::serve_with_ports("dev", path.to_str().unwrap(), ports).await;

        port_listing(dir.path(), &mut client, format).await.unwrap()
    }

    fn json(listing: &Listing) -> serde_json::Value {
        serde_json::from_str(&listing.output).unwrap()
    }

    #[tokio::test]
    async fn ls_merges_the_forwards_with_the_listening_ports() {
        let listing = list_dev(running_ports(), OutputFormat::Json).await;

        assert_eq!(
            json(&listing),
            serde_json::json!([
                {
                    "host": 3000,
                    "guest": 3000,
                    "addresses": ["127.0.0.1"],
                    "process": { "name": "node", "pid": 412 },
                    "status": "forwarded"
                },
                {
                    "host": null,
                    "guest": 5173,
                    "addresses": ["0.0.0.0", "::"],
                    "process": { "name": "vite", "pid": 530 },
                    "status": "listening"
                },
                {
                    "host": 8080,
                    "guest": 8081,
                    "addresses": [],
                    "process": null,
                    "status": "not listening"
                }
            ])
        );
        assert!(listing.note.is_none(), "{:?}", listing.note);
    }

    /// Returns the trimmed cells of a table line.
    fn cells(line: &str) -> Vec<String> {
        line.split('│')
            .map(|cell| cell.trim().to_string())
            .filter(|cell| !cell.is_empty())
            .collect()
    }

    #[tokio::test]
    async fn ls_table_shows_a_dash_for_empty_cells() {
        let listing = list_dev(running_ports(), OutputFormat::Table).await;
        let lines: Vec<&str> = listing.output.lines().collect();

        assert_eq!(
            cells(lines[1]),
            ["HOST", "GUEST", "ADDRESS", "PROCESS", "STATUS"]
        );
        assert_eq!(
            cells(lines[3]),
            ["3000", "3000", "127.0.0.1", "node (412)", "forwarded"]
        );
        assert_eq!(
            cells(lines[4]),
            ["-", "5173", "0.0.0.0, ::", "vite (530)", "listening"]
        );
        assert_eq!(cells(lines[5]), ["8080", "8081", "-", "-", "not listening"]);
    }

    #[tokio::test]
    async fn ls_of_a_stopped_sandbox_lists_its_forwards_as_stopped() {
        let ports = ListPortsResponse {
            running: false,
            forwards: vec![stored(8080, 80)],
            ..Default::default()
        };

        let listing = list_dev(ports, OutputFormat::Json).await;

        assert_eq!(json(&listing)[0]["status"], "stopped");
        assert_eq!(json(&listing).as_array().unwrap().len(), 1);
        assert_eq!(
            listing.note.as_deref(),
            Some("sandbox dev isn't running; start it to see its listening ports")
        );
    }

    #[tokio::test]
    async fn ls_warns_when_the_listening_ports_could_not_be_read() {
        let ports = ListPortsResponse {
            running: true,
            forwards: vec![stored(8080, 80)],
            listeners: vec![],
            listeners_error: Some("the guest didn't answer within\u{1b}[2J 5 seconds".to_string()),
            listeners_truncated: false,
        };

        let listing = list_dev(ports, OutputFormat::Json).await;

        assert_eq!(json(&listing)[0]["status"], "unknown");
        assert_eq!(
            listing.note.as_deref(),
            Some(
                "warning: couldn't read the listening ports of sandbox dev: the guest didn't \
                 answer within 5 seconds"
            )
        );
    }

    #[tokio::test]
    async fn ls_warns_when_the_listening_ports_are_incomplete() {
        let ports = ListPortsResponse {
            listeners_truncated: true,
            ..running_ports()
        };

        let listing = list_dev(ports, OutputFormat::Json).await;

        assert_eq!(json(&listing)[0]["status"], "forwarded");
        assert_eq!(
            listing.note.as_deref(),
            Some(
                "warning: sandbox dev reported more than fbk reads; the list may miss listening \
                 ports or their processes"
            )
        );
    }

    #[tokio::test]
    async fn ls_strips_control_characters_from_process_names() {
        let ports = ListPortsResponse {
            running: true,
            listeners: vec![guest_listener(
                80,
                &["0.0.0.0"],
                Some((7, "evil\u{1b}[2J\nname")),
            )],
            ..Default::default()
        };

        let listing = list_dev(ports, OutputFormat::Table).await;

        assert!(!listing.output.contains('\u{1b}'), "{:?}", listing.output);
        assert!(
            listing.output.contains("evil name (7)"),
            "{}",
            listing.output
        );
    }

    #[tokio::test]
    async fn ls_without_forwards_or_listeners_says_so() {
        let ports = ListPortsResponse {
            running: true,
            ..Default::default()
        };

        let listing = list_dev(ports, OutputFormat::Table).await;

        assert_eq!(
            listing.output,
            "Sandbox dev has no forwards or listening ports.\n"
        );
    }

    #[tokio::test]
    async fn ls_of_a_sandbox_that_does_not_exist_fails() {
        let dir = workspace();
        let mut client = fake_daemon::serve("other", OTHER_WORKSPACE).await;

        let err = port_listing(dir.path(), &mut client, OutputFormat::Table)
            .await
            .unwrap_err();

        assert_eq!(err.to_string(), "sandbox dev doesn't exist");
    }

    #[tokio::test]
    async fn ls_refuses_sandbox_of_other_workspace() {
        let dir = workspace();
        let mut client =
            fake_daemon::serve_with_ports("dev", OTHER_WORKSPACE, running_ports()).await;

        let result = port_listing(dir.path(), &mut client, OutputFormat::Table).await;

        fake_daemon::assert_dev_belongs_to_other(result);
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
