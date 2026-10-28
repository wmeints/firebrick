//! Records the domains and IP addresses that sandboxes contact.
//!
//! microsandbox doesn't report the DNS queries or connections of a sandbox, so a logger in the
//! `firebrick-base` image watches the guest's network interface and writes one JSON event per
//! line to vsock port [`GUEST_PORT`] on the host. Each sandbox gets a stream route from that port
//! to its own unix socket in [`firebrick_utils::netlog_socket_dir`], where fbkd listens. fbkd
//! treats every line as untrusted input: the logger runs inside the VM, so the agent can stop it
//! or send false events. It aggregates the valid events per destination into a record file in
//! [`firebrick_utils::network_log_dir`]. The record is a diagnostic aid, not an audit trail.

use crate::guest::vsock::{self, Connections, Listeners, Refused};
use chrono::{DateTime, Utc};
use firebrick_utils::ipc;
use microsandbox::sandbox::{SandboxBuilder, SandboxSpec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;

mod parse;
mod record;

use parse::read_events;
use record::{FLUSH_INTERVAL, Recorder, Source, read_record_in_background};

/// Guest vsock port the logger in the guest writes its events to, on host CID 2.
pub const GUEST_PORT: u32 = 52000;

/// The name of the channel in log lines.
const CHANNEL: &str = "network log";

/// The transport protocol of a connection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// A TCP SYN.
    Tcp,
    /// The first UDP datagram to a destination.
    Udp,
}

/// A DNS name a sandbox resolved, and the IPv4 addresses the answers held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsEntry {
    /// The name, lowercase and without a trailing dot.
    pub name: String,
    /// The addresses of the A records in the answers.
    pub addresses: BTreeSet<Ipv4Addr>,
    /// How often the name was looked up.
    pub count: u64,
    /// When fbkd first received a lookup of the name.
    pub first_seen: DateTime<Utc>,
    /// When fbkd last received a lookup of the name.
    pub last_seen: DateTime<Utc>,
}

/// An IP address a sandbox tried to connect to with one protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionEntry {
    /// The destination address.
    pub ip: Ipv4Addr,
    /// The transport protocol.
    pub protocol: Protocol,
    /// The destination ports.
    pub ports: BTreeSet<u16>,
    /// How often the sandbox tried to connect.
    pub count: u64,
    /// When fbkd first received an attempt.
    pub first_seen: DateTime<Utc>,
    /// When fbkd last received an attempt.
    pub last_seen: DateTime<Utc>,
}

/// The network activity recorded for a sandbox, one entry per destination.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The DNS names the sandbox resolved, sorted by name.
    pub dns: Vec<DnsEntry>,
    /// The connection attempts of the sandbox, sorted by address and protocol.
    pub connections: Vec<ConnectionEntry>,
}

/// Why a record file couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// The file exists but couldn't be read.
    #[error("failed to read {}: {source}", path.display())]
    Read {
        /// The record file.
        path: PathBuf,
        /// Why reading failed.
        source: io::Error,
    },
    /// The file isn't a valid record.
    #[error("{} is corrupt: {source}", path.display())]
    Corrupt {
        /// The record file.
        path: PathBuf,
        /// Why parsing failed.
        source: serde_json::Error,
    },
}

/// Records the events of an accepted connection, or closes it with a warning when the sandbox
/// already has [`vsock::MAX_CONNECTIONS`] connections.
fn accept(
    connections: &mut Connections,
    accepted: io::Result<ipc::Stream>,
    recorder: &Arc<Recorder>,
) {
    let read = |stream| read_events(stream, Arc::clone(recorder));

    match connections.accept(accepted, read) {
        Ok(()) => {}
        Err(Refused::Full) => recorder.warn(
            Source::Guest,
            &format!(
                "closed a connection beyond the limit of {}",
                vsock::MAX_CONNECTIONS
            ),
        ),
        Err(Refused::Failed(err)) => recorder.warn(
            Source::Host,
            &format!("failed to accept a connection: {err}"),
        ),
    }
}

/// Logs a connection task that panicked.
fn finished(recorder: &Recorder, result: Result<(), tokio::task::JoinError>) {
    if let Some(err) = vsock::panicked(result) {
        tracing::error!(
            "a network log connection of sandbox {} failed: {err}",
            recorder.sandbox
        );
    }
}

/// Accepts connections from the logger on the listener and records their events, writing the
/// record to its file every [`FLUSH_INTERVAL`] while it changes. When `shutdown` fires or its
/// sender is dropped, it writes the record a last time, unless the sandbox was removed, and
/// returns, which closes its connections.
async fn serve(
    mut listener: ipc::Listener,
    recorder: Arc<Recorder>,
    shutdown: oneshot::Receiver<()>,
) {
    let mut connections = Connections::default();
    let mut flush = tokio::time::interval(FLUSH_INTERVAL);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => accept(&mut connections, accepted, &recorder),
            _ = flush.tick() => recorder.flush().await,
            Some(result) = connections.join_next() => finished(&recorder, result),
            _ = &mut shutdown => break,
        }
    }

    recorder.flush().await;
}

/// Listens for the network events of sandboxes and keeps their records.
pub struct NetLogs {
    listeners: Listeners<Recorder>,
    record_dir: Option<PathBuf>,
}

impl NetLogs {
    /// Creates a registry with the sockets in `socket_dir` and the records in `record_dir`.
    /// Without a socket directory, sandboxes get no network log; without a record directory,
    /// records are kept in memory only.
    pub fn new(socket_dir: Option<PathBuf>, record_dir: Option<PathBuf>) -> Self {
        Self {
            listeners: Listeners::new(CHANNEL, GUEST_PORT, socket_dir),
            record_dir,
        }
    }

    /// Creates a registry in the directories the environment selects, see
    /// [`firebrick_utils::netlog_socket_dir`] and [`firebrick_utils::network_log_dir`]. Logs a
    /// warning for each one the environment doesn't provide, and leaves out a socket directory
    /// that isn't private to the user.
    pub fn from_env() -> Self {
        let socket_dir = firebrick_utils::netlog_socket_dir();
        let record_dir = firebrick_utils::network_log_dir();

        if record_dir.is_none() {
            tracing::warn!("there's no state directory, so network records aren't saved");
        }

        Self {
            listeners: Listeners::private(CHANNEL, GUEST_PORT, socket_dir),
            record_dir,
        }
    }

    /// Returns the socket of the sandbox, or `None` when there's no socket directory or the
    /// path is too long for a unix socket.
    pub fn socket_path(&self, sandbox: &str) -> Option<PathBuf> {
        self.listeners.socket_path(sandbox)
    }

    /// Returns the record file of the sandbox, if there's a record directory and the name is a
    /// single path component.
    fn record_path(&self, sandbox: &str) -> Option<PathBuf> {
        vsock::sandbox_file(self.record_dir.as_deref()?, sandbox, "json")
    }

    /// Adds the route from [`GUEST_PORT`] to the sandbox's socket to the builder. Leaves the
    /// builder alone, with a warning, when the sandbox has no socket path, for example because
    /// it would be too long, since microsandbox would then refuse to boot the sandbox.
    pub fn add_route(&self, builder: SandboxBuilder, sandbox: &str) -> SandboxBuilder {
        self.listeners.add_route(builder, sandbox)
    }

    /// Whether the sandbox has a stream route from [`GUEST_PORT`] to its socket.
    pub fn has_route(&self, sandbox: &str, spec: &SandboxSpec) -> bool {
        self.listeners.has_route(sandbox, spec)
    }

    /// Listens on the sandbox's socket unless fbkd already does, loading its record. A listener
    /// that stopped, for example because it panicked, is replaced. Removes a stale socket file
    /// first, but leaves one alone that another process listens on. Logs an error when the
    /// socket can't be bound: network logging never stops a sandbox from starting.
    pub async fn listen(&self, sandbox: &str) {
        let recorder = Recorder::load(sandbox, self.record_path(sandbox));
        let listened = self.listeners.listen(sandbox, recorder, serve).await;

        if let Err(err) = listened {
            tracing::error!("failed to listen for the network log of sandbox {sandbox}: {err}");
        }
    }

    /// Returns the network activity recorded for the sandbox: the live record while fbkd
    /// listens for it, otherwise its record file, which is empty when there's none. Fails when
    /// the file can't be read or is corrupt.
    pub async fn record(&self, sandbox: &str) -> Result<Record, RecordError> {
        let live = self
            .listeners
            .state(sandbox)
            .map(|recorder| recorder.record());

        match (live, self.record_path(sandbox)) {
            (Some(record), _) => Ok(record),
            (None, Some(path)) => read_record_in_background(path).await,
            (None, None) => Ok(Record::default()),
        }
    }

    /// Stops listening for the sandbox, waits until its listener has stopped writing, and
    /// deletes its socket and record. Logs a warning when a file can't be deleted.
    pub async fn remove(&self, sandbox: &str) {
        self.listeners.remove(sandbox, Recorder::mark_removed).await;
        vsock::remove_logged(self.record_path(sandbox)).await;
    }

    /// Stops listening for every sandbox, writing their records a last time. Run when the
    /// daemon shuts down.
    pub async fn close(&self) {
        self.listeners.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::parse::{Event, MAX_LINE};
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::{Duration, Instant};
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    pub(super) fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    pub(super) fn ip(text: &str) -> Ipv4Addr {
        text.parse().unwrap()
    }

    pub(super) fn connect(ip_text: &str, port: u16) -> Event {
        Event::Connect {
            protocol: Protocol::Tcp,
            ip: ip(ip_text),
            port,
        }
    }

    pub(super) fn dns(name: &str, addresses: &[&str]) -> Event {
        Event::Dns {
            name: name.to_string(),
            addresses: addresses.iter().map(|a| ip(a)).collect(),
        }
    }

    const CONNECT_LINE: &str = r#"{"kind":"connect","protocol":"tcp","ip":"1.1.1.1","port":443}"#;

    /// Returns a registry with its sockets and records in a temp directory.
    pub(super) fn netlogs(dir: &Path) -> NetLogs {
        NetLogs::new(
            Some(vsock::test_endpoint_dir(dir, "netlog")),
            Some(dir.join("network")),
        )
    }

    /// Connects to the sandbox's socket like the logger in the guest.
    async fn connect_logger(logs: &NetLogs, sandbox: &str) -> io::Result<ipc::Stream> {
        ipc::connect(&logs.socket_path(sandbox).unwrap()).await
    }

    /// Opens `count` connections to the sandbox's socket.
    async fn connect_many(logs: &NetLogs, count: usize) -> Vec<ipc::Stream> {
        let mut streams = vec![];

        for _ in 0..count {
            streams.push(connect_logger(logs, "sb").await.unwrap());
        }

        streams
    }

    /// Writes the lines, each followed by a newline, to the stream.
    async fn write_lines(stream: &mut ipc::Stream, lines: &[&str]) {
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        stream.write_all(text.as_bytes()).await.unwrap();
    }

    /// Waits up to 5 seconds until the record of the sandbox has the number of destinations.
    async fn wait_for_destinations(logs: &NetLogs, sandbox: &str, expected: usize) -> Record {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut record = logs.record(sandbox).await.unwrap();

        while record.dns.len() + record.connections.len() != expected {
            assert!(Instant::now() < deadline, "record of {sandbox}: {record:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
            record = logs.record(sandbox).await.unwrap();
        }

        record
    }

    #[tokio::test]
    async fn records_valid_lines_and_keeps_the_connection_after_invalid_ones() {
        let dir = tempfile::tempdir().unwrap();
        let logs = netlogs(dir.path());
        logs.listen("sb").await;
        let mut stream = connect_logger(&logs, "sb").await.unwrap();
        let long = "x".repeat(MAX_LINE + 1);
        let dns_line = r#"{"kind":"dns","name":"example.com","addresses":["1.1.1.1"]}"#;

        write_lines(&mut stream, &[dns_line, "garbage", &long, CONNECT_LINE]).await;

        let record = wait_for_destinations(&logs, "sb", 2).await;
        assert_eq!(record.dns[0].name, "example.com");
        assert_eq!(record.connections[0].ports, [443].into());
    }

    #[tokio::test]
    async fn closes_connections_beyond_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let logs = netlogs(dir.path());
        logs.listen("sb").await;
        let mut kept = connect_many(&logs, vsock::MAX_CONNECTIONS).await;

        let mut extra = connect_logger(&logs, "sb").await.unwrap();
        let mut byte = [0; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), extra.read(&mut byte)).await;
        write_lines(&mut kept[0], &[CONNECT_LINE]).await;

        assert_eq!(
            read.unwrap().unwrap(),
            0,
            "the extra connection stayed open"
        );
        wait_for_destinations(&logs, "sb", 1).await;
    }

    #[tokio::test]
    async fn close_writes_the_record_privately_in_rfc_3339() {
        let dir = tempfile::tempdir().unwrap();
        let logs = netlogs(dir.path());
        logs.listen("sb").await;
        let mut stream = connect_logger(&logs, "sb").await.unwrap();
        write_lines(&mut stream, &[CONNECT_LINE]).await;
        let record = wait_for_destinations(&logs, "sb", 1).await;

        logs.close().await;

        let path = dir.path().join("network/sb.json");
        #[cfg(unix)]
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let first_seen = json["connections"][0]["first_seen"].as_str().unwrap();
        #[cfg(unix)]
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            DateTime::parse_from_rfc3339(first_seen).is_ok(),
            "{first_seen}"
        );
        assert_eq!(netlogs(dir.path()).record("sb").await.unwrap(), record);
    }

    #[tokio::test]
    async fn listen_leaves_a_live_socket_alone_and_replaces_a_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let first = netlogs(dir.path());
        first.listen("sb").await;
        let second = netlogs(dir.path());

        second.listen("sb").await;
        write_lines(
            &mut connect_logger(&first, "sb").await.unwrap(),
            &[CONNECT_LINE],
        )
        .await;
        wait_for_destinations(&first, "sb", 1).await;

        first.close().await;
        second.listen("sb").await;
        let dns_line = r#"{"kind":"dns","name":"example.com","addresses":[]}"#;
        write_lines(
            &mut connect_logger(&second, "sb").await.unwrap(),
            &[dns_line],
        )
        .await;

        // The second listener continues the record the first one wrote when it closed.
        let record = wait_for_destinations(&second, "sb", 2).await;
        assert_eq!(record.dns[0].name, "example.com");
        assert_eq!(record.connections[0].ip, ip("1.1.1.1"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn listen_without_a_usable_socket_directory_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("netlog"), "not a directory").unwrap();
        let logs = netlogs(dir.path());

        logs.listen("sb").await;

        assert!(connect_logger(&logs, "sb").await.is_err());
        assert_eq!(logs.record("sb").await.unwrap(), Record::default());
    }

    #[tokio::test]
    async fn remove_deletes_the_socket_and_the_record_for_good() {
        let dir = tempfile::tempdir().unwrap();
        let logs = netlogs(dir.path());
        logs.listen("sb").await;
        let mut stream = connect_logger(&logs, "sb").await.unwrap();
        write_lines(&mut stream, &[CONNECT_LINE]).await;
        wait_for_destinations(&logs, "sb", 1).await;

        logs.remove("sb").await;
        tokio::time::sleep(FLUSH_INTERVAL * 2).await;

        assert!(!ipc::exists(&logs.socket_path("sb").unwrap()).await);
        assert!(!dir.path().join("network/sb.json").exists());
        assert_eq!(logs.record("sb").await.unwrap(), Record::default());
    }

    /// Writes a record with one connection to `outside.json` in the directory, outside the
    /// record directory, and returns its content.
    async fn write_outside_record(dir: &Path) -> Vec<u8> {
        let logs = netlogs(dir);
        logs.listen("sb").await;
        let mut stream = connect_logger(&logs, "sb").await.unwrap();
        write_lines(&mut stream, &[CONNECT_LINE]).await;
        wait_for_destinations(&logs, "sb", 1).await;
        logs.close().await;

        let outside = dir.join("outside.json");
        std::fs::rename(dir.join("network/sb.json"), &outside).unwrap();
        std::fs::read(outside).unwrap()
    }

    #[tokio::test]
    async fn a_name_that_leaves_the_directories_touches_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let content = write_outside_record(dir.path()).await;
        let logs = netlogs(dir.path());

        logs.listen("../outside").await;
        let socket_created = dir.path().join("outside.sock").exists();
        let record = logs.record("../outside").await.unwrap();
        logs.remove("../outside").await;

        assert!(!socket_created);
        assert_eq!(record, Record::default());
        assert_eq!(
            std::fs::read(dir.path().join("outside.json")).unwrap(),
            content
        );
    }
}
