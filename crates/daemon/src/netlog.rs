//! Records the domains and IP addresses that sandboxes contact.
//!
//! microsandbox doesn't report the DNS queries or connections of a sandbox, so a logger in the
//! `firebrick-base` image watches the guest's network interface and writes one JSON event per
//! line to vsock port [`GUEST_PORT`] on the host. Each sandbox gets a stream route from that port
//! to its own unix socket in [`firebrick_utils::netlog_socket_dir`], where fbkd listens. fbkd
//! treats every line as untrusted input: the logger runs inside the VM, so the agent can stop it
//! or send false events. It aggregates the valid events per destination into a record file in
//! [`firebrick_utils::network_log_dir`]. The record is a diagnostic aid, not an audit trail.

use crate::vsock::{self, Connections, Listeners, Refused};
use chrono::{DateTime, Utc};
use firebrick_utils::strip_control;
use microsandbox::sandbox::{SandboxBuilder, SandboxSpec};
use serde::{Deserialize, Serialize};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

/// Guest vsock port the logger in the guest writes its events to, on host CID 2.
pub const GUEST_PORT: u32 = 52000;

/// The name of the channel in log lines.
const CHANNEL: &str = "network log";

/// The longest line fbkd reads from the logger, in bytes, without its newline.
const MAX_LINE: usize = 4 * 1024;

/// The most destinations, DNS names and connection targets together, a record holds.
const MAX_DESTINATIONS: usize = 10_000;

/// The most addresses or ports one destination of a record holds.
const MAX_VALUES: usize = 64;

/// How often fbkd logs a warning about invalid lines from one sandbox at most.
const WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// How often fbkd writes a changed record to its file.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

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

/// An event the logger in the guest reports. Fields other than these, such as the guest's
/// timestamp `at`, are ignored: fbkd records when it received the event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Event {
    /// A DNS query and the IPv4 addresses in its answer.
    Dns {
        name: String,
        addresses: Vec<Ipv4Addr>,
    },
    /// An outbound connection attempt.
    Connect {
        protocol: Protocol,
        ip: Ipv4Addr,
        port: u16,
    },
}

/// Why fbkd dropped a line from the logger.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Invalid {
    /// The line is longer than [`MAX_LINE`] bytes.
    TooLong,
    /// The line isn't a JSON event of a known kind with valid fields.
    Malformed(String),
    /// The name of a DNS event isn't a valid DNS name.
    Name,
    /// The port of a connect event is 0.
    Port,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Invalid::TooLong => write!(f, "the line is longer than {MAX_LINE} bytes"),
            Invalid::Malformed(err) => write!(f, "{}", strip_control(err)),
            Invalid::Name => write!(f, "the name isn't a valid DNS name"),
            Invalid::Port => write!(f, "the port is 0"),
        }
    }
}

/// Parses and checks a line from the logger, without its newline. Returns the event with its
/// DNS name in lowercase and without a trailing dot.
fn parse_line(line: &[u8]) -> Result<Event, Invalid> {
    if line.len() > MAX_LINE {
        return Err(Invalid::TooLong);
    }

    match serde_json::from_slice(line).map_err(|err| Invalid::Malformed(err.to_string()))? {
        Event::Dns { name, addresses } => Ok(Event::Dns {
            name: dns_name(&name).ok_or(Invalid::Name)?,
            addresses,
        }),
        Event::Connect { port: 0, .. } => Err(Invalid::Port),
        event @ Event::Connect { .. } => Ok(event),
    }
}

/// Returns the name in lowercase and without a trailing dot when it's a valid DNS name: at most
/// 253 bytes of dot-separated labels of 1 to 63 ASCII letters, digits, dashes or underscores.
///
/// This is looser than the host name check of allow rules in `firebrick-spec`, on purpose: it
/// records what the guest looked up, and real lookups include names such as `_dmarc.example.com`
/// that are valid in DNS but no host name an allow rule would name.
fn dns_name(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    let valid = !name.is_empty() && name.len() <= 253 && name.split('.').all(is_dns_label);

    valid.then(|| name.to_ascii_lowercase())
}

/// Whether the label is 1 to 63 ASCII letters, digits, dashes or underscores.
fn is_dns_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The record already holds [`MAX_DESTINATIONS`] destinations, so a new one was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Full;

/// The entries of a record by destination, so events can update them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Aggregate {
    dns: BTreeMap<String, DnsEntry>,
    connections: BTreeMap<(Ipv4Addr, Protocol), ConnectionEntry>,
}

impl Aggregate {
    /// Indexes the entries of a record by destination.
    fn from_record(record: Record) -> Self {
        Self {
            dns: record
                .dns
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect(),
            connections: record
                .connections
                .into_iter()
                .map(|entry| ((entry.ip, entry.protocol), entry))
                .collect(),
        }
    }

    /// Returns the entries as a record, sorted by destination.
    fn to_record(&self) -> Record {
        Record {
            dns: self.dns.values().cloned().collect(),
            connections: self.connections.values().cloned().collect(),
        }
    }

    /// Adds the event received at `now` to the entry of its destination. Fails when the
    /// destination is new and the record is full.
    fn apply(&mut self, event: Event, now: DateTime<Utc>) -> Result<(), Full> {
        let full = self.dns.len() + self.connections.len() >= MAX_DESTINATIONS;

        match event {
            Event::Dns { name, addresses } => {
                let key = name.clone();
                let entry = upsert(&mut self.dns, key, full, move || DnsEntry::new(name, now))?;
                extend_capped(&mut entry.addresses, addresses);
                entry.count = entry.count.saturating_add(1);
                entry.last_seen = now;
            }
            Event::Connect { protocol, ip, port } => {
                let new = || ConnectionEntry::new(ip, protocol, now);
                let entry = upsert(&mut self.connections, (ip, protocol), full, new)?;
                extend_capped(&mut entry.ports, [port]);
                entry.count = entry.count.saturating_add(1);
                entry.last_seen = now;
            }
        }

        Ok(())
    }
}

impl DnsEntry {
    /// Returns an entry for the name, first seen at `now`, without lookups yet.
    fn new(name: String, now: DateTime<Utc>) -> Self {
        Self {
            name,
            addresses: BTreeSet::new(),
            count: 0,
            first_seen: now,
            last_seen: now,
        }
    }
}

impl ConnectionEntry {
    /// Returns an entry for the destination, first seen at `now`, without attempts yet.
    fn new(ip: Ipv4Addr, protocol: Protocol, now: DateTime<Utc>) -> Self {
        Self {
            ip,
            protocol,
            ports: BTreeSet::new(),
            count: 0,
            first_seen: now,
            last_seen: now,
        }
    }
}

/// Returns the entry with the key, adding a new one unless the map is `full`.
fn upsert<K: Ord, V>(
    map: &mut BTreeMap<K, V>,
    key: K,
    full: bool,
    new: impl FnOnce() -> V,
) -> Result<&mut V, Full> {
    match map.entry(key) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(_) if full => Err(Full),
        Entry::Vacant(entry) => Ok(entry.insert(new())),
    }
}

/// Adds the values to the set until it holds [`MAX_VALUES`] values.
fn extend_capped<T: Ord>(set: &mut BTreeSet<T>, values: impl IntoIterator<Item = T>) {
    for value in values {
        if set.len() >= MAX_VALUES {
            return;
        }

        set.insert(value);
    }
}

/// Where a warning about a sandbox's network log comes from. Each source has its own rate
/// limit, so lines from the guest can't hide failures on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// Input from the guest, such as an invalid line or too many connections.
    Guest,
    /// A failure on the host, such as a record that can't be written.
    Host,
}

/// The record of one sandbox while fbkd listens for its events.
struct Recorder {
    sandbox: String,
    path: Option<PathBuf>,
    state: Mutex<RecorderState>,
}

/// The mutable state of a [`Recorder`].
struct RecorderState {
    aggregate: Aggregate,
    /// Whether the aggregate changed since it was last written.
    dirty: bool,
    /// Whether the sandbox was removed, so the record must not be written anymore.
    removed: bool,
    /// When fbkd last logged a warning about input from the guest.
    guest_warning: Option<Instant>,
    /// When fbkd last logged a warning about a failure on the host.
    host_warning: Option<Instant>,
    /// Whether fbkd logged that the record is full.
    warned_full: bool,
}

impl Recorder {
    /// Loads the record of the sandbox from `path` on a blocking thread, or starts an empty one
    /// when there's no file or it's corrupt. Without a path, the record is kept in memory only.
    async fn load(sandbox: &str, path: Option<PathBuf>) -> Self {
        let record = match path.clone() {
            Some(path) => load_record_in_background(sandbox.to_string(), path).await,
            None => Record::default(),
        };

        Self {
            sandbox: sandbox.to_string(),
            path,
            state: Mutex::new(RecorderState {
                aggregate: Aggregate::from_record(record),
                dirty: false,
                removed: false,
                guest_warning: None,
                host_warning: None,
                warned_full: false,
            }),
        }
    }

    /// Locks the state. It stays consistent when a thread panics while holding it, so a
    /// poisoned lock is safe to use.
    fn state(&self) -> MutexGuard<'_, RecorderState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns the entries recorded so far.
    fn record(&self) -> Record {
        self.state().aggregate.to_record()
    }

    /// Records a line from the logger, or drops it with a rate-limited warning when it's
    /// invalid.
    fn handle_line(&self, line: &[u8]) {
        match parse_line(line) {
            Ok(event) => self.add(event),
            Err(invalid) => self.drop_invalid(&invalid),
        }
    }

    /// Drops an invalid line with a rate-limited warning.
    fn drop_invalid(&self, invalid: &Invalid) {
        self.warn(
            Source::Guest,
            &format!("dropped an invalid event: {invalid}"),
        );
    }

    /// Adds the event to the record, logging a warning once when it's full.
    fn add(&self, event: Event) {
        let mut state = self.state();

        match state.aggregate.apply(event, Utc::now()) {
            Ok(()) => state.dirty = true,
            Err(Full) if !state.warned_full => {
                state.warned_full = true;
                tracing::warn!(
                    "the network record of sandbox {} holds {MAX_DESTINATIONS} destinations; \
                     dropping new ones",
                    self.sandbox
                );
            }
            Err(Full) => {}
        }
    }

    /// Logs a warning about the sandbox, at most once per [`WARNING_INTERVAL`] per source.
    fn warn(&self, source: Source, message: &str) {
        let mut state = self.state();
        let last = match source {
            Source::Guest => &mut state.guest_warning,
            Source::Host => &mut state.host_warning,
        };
        let now = Instant::now();

        if last.is_some_and(|last| now.duration_since(last) < WARNING_INTERVAL) {
            return;
        }

        *last = Some(now);
        tracing::warn!("network log of sandbox {}: {message}", self.sandbox);
    }

    /// Marks the sandbox as removed, so the record is never written again.
    fn mark_removed(&self) {
        self.state().removed = true;
    }

    /// Writes the record to its file when it changed since the last write.
    async fn flush(&self) {
        let (Some(path), Some(content)) = (self.path.clone(), self.take_changes()) else {
            return;
        };
        let written = match content {
            Ok(content) => write_in_background(path, content).await,
            Err(err) => Err(io::Error::other(err)),
        };

        if let Err(err) = written {
            self.warn(Source::Host, &format!("failed to write the record: {err}"));
        }
    }

    /// Returns the record as JSON when it changed since the last call and the sandbox wasn't
    /// removed.
    fn take_changes(&self) -> Option<serde_json::Result<String>> {
        let mut state = self.state();
        let dirty = std::mem::replace(&mut state.dirty, false) && !state.removed;

        dirty.then(|| serde_json::to_string(&state.aggregate.to_record()))
    }
}

/// Writes the private file on a blocking thread, so a large record doesn't stall the runtime.
async fn write_in_background(path: PathBuf, content: String) -> io::Result<()> {
    tokio::task::spawn_blocking(move || firebrick_utils::write_private(&path, &content))
        .await
        .unwrap_or_else(|err| Err(io::Error::other(err)))
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

/// Reads the record file like [`read_record`], on a blocking thread.
async fn read_record_in_background(path: PathBuf) -> Result<Record, RecordError> {
    let read_path = path.clone();

    tokio::task::spawn_blocking(move || read_record(&read_path))
        .await
        .unwrap_or_else(|err| {
            Err(RecordError::Read {
                path,
                source: io::Error::other(err),
            })
        })
}

/// Reads the record file like [`read_record`], on a blocking thread, or returns an empty record
/// with a warning when it can't be read or is corrupt.
async fn load_record_in_background(sandbox: String, path: PathBuf) -> Record {
    read_record_in_background(path).await.unwrap_or_else(|err| {
        tracing::warn!(
            "the network record of sandbox {sandbox} can't be used; starting a new one: {err}"
        );
        Record::default()
    })
}

/// Reads the record file. A missing file is an empty record.
fn read_record(path: &Path) -> Result<Record, RecordError> {
    let content = match std::fs::read(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Record::default()),
        Err(source) => {
            return Err(RecordError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    serde_json::from_slice(&content).map_err(|source| RecordError::Corrupt {
        path: path.to_path_buf(),
        source,
    })
}

/// A line read from the logger.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// A line of at most [`MAX_LINE`] bytes, now in the buffer without its newline.
    Complete,
    /// A line longer than [`MAX_LINE`] bytes, which was skipped.
    TooLong,
    /// The connection ended.
    End,
}

/// Reads the next line into `buf`, reading at most [`MAX_LINE`] bytes of it into memory.
async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> io::Result<Line> {
    buf.clear();
    let limit = u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX);
    let read = (&mut *reader).take(limit).read_until(b'\n', buf).await?;

    if read == 0 {
        return Ok(Line::End);
    }

    if buf.last() == Some(&b'\n') {
        buf.pop();
    }

    if buf.len() <= MAX_LINE {
        return Ok(Line::Complete);
    }

    skip_line(reader).await?;

    Ok(Line::TooLong)
}

/// Skips the rest of the current line, including its newline.
async fn skip_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<()> {
    loop {
        let available = reader.fill_buf().await?;

        if available.is_empty() {
            return Ok(());
        }

        let (consumed, done) = match available.iter().position(|&b| b == b'\n') {
            Some(newline) => (newline + 1, true),
            None => (available.len(), false),
        };
        reader.consume(consumed);

        if done {
            return Ok(());
        }
    }
}

/// Records the lines of one connection from the logger until it closes.
async fn read_events(stream: UnixStream, recorder: Arc<Recorder>) {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::with_capacity(MAX_LINE + 1);

    loop {
        match read_line(&mut reader, &mut buf).await {
            Ok(Line::Complete) => recorder.handle_line(&buf),
            Ok(Line::TooLong) => recorder.drop_invalid(&Invalid::TooLong),
            Ok(Line::End) => return,
            Err(err) => {
                recorder.warn(Source::Guest, &format!("failed to read events: {err}"));
                return;
            }
        }
    }
}

/// Records the events of an accepted connection, or closes it with a warning when the sandbox
/// already has [`vsock::MAX_CONNECTIONS`] connections.
fn accept(
    connections: &mut Connections,
    accepted: io::Result<(UnixStream, tokio::net::unix::SocketAddr)>,
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
async fn serve(listener: UnixListener, recorder: Arc<Recorder>, shutdown: oneshot::Receiver<()>) {
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
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    fn ip(text: &str) -> Ipv4Addr {
        text.parse().unwrap()
    }

    fn connect(ip_text: &str, port: u16) -> Event {
        Event::Connect {
            protocol: Protocol::Tcp,
            ip: ip(ip_text),
            port,
        }
    }

    fn dns(name: &str, addresses: &[&str]) -> Event {
        Event::Dns {
            name: name.to_string(),
            addresses: addresses.iter().map(|a| ip(a)).collect(),
        }
    }

    #[test]
    fn parses_dns_and_connect_events() {
        let dns_line = br#"{"kind":"dns","name":"API.GitHub.com.","addresses":["140.82.112.6"],"at":"2026-10-11T14:02:11Z"}"#;
        let connect_line = br#"{"kind":"connect","protocol":"udp","ip":"140.82.112.6","port":443}"#;

        assert_eq!(
            parse_line(dns_line),
            Ok(dns("api.github.com", &["140.82.112.6"]))
        );
        assert_eq!(
            parse_line(connect_line),
            Ok(Event::Connect {
                protocol: Protocol::Udp,
                ip: ip("140.82.112.6"),
                port: 443
            })
        );
    }

    #[test]
    fn rejects_malformed_events() {
        let lines: [&[u8]; 6] = [
            b"not json",
            br#"{"kind":"http","url":"https://example.com"}"#,
            br#"{"kind":"dns","name":"example.com","addresses":["::1"]}"#,
            br#"{"kind":"connect","protocol":"tcp","ip":"300.1.1.1","port":443}"#,
            br#"{"kind":"connect","protocol":"tcp","ip":"1.1.1.1","port":70000}"#,
            br#"{"kind":"connect","protocol":"icmp","ip":"1.1.1.1","port":1}"#,
        ];

        for line in lines {
            let result = parse_line(line);
            let text = String::from_utf8_lossy(line);
            assert!(
                matches!(result, Err(Invalid::Malformed(_))),
                "{text}: {result:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_names_ports_and_long_lines() {
        let cases: [(&[u8], Invalid); 5] = [
            (
                br#"{"kind":"dns","name":"bad name","addresses":[]}"#,
                Invalid::Name,
            ),
            (br#"{"kind":"dns","name":"","addresses":[]}"#, Invalid::Name),
            (
                br#"{"kind":"dns","name":"a..b","addresses":[]}"#,
                Invalid::Name,
            ),
            (
                br#"{"kind":"connect","protocol":"tcp","ip":"1.1.1.1","port":0}"#,
                Invalid::Port,
            ),
            (&[b' '; MAX_LINE + 1], Invalid::TooLong),
        ];

        for (line, expected) in cases {
            assert_eq!(
                parse_line(line),
                Err(expected),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn dns_name_checks_label_and_name_lengths() {
        let label = "a".repeat(63);
        let name_253 = format!("{0}.{0}.{0}.{1}", label, "a".repeat(61));
        let name_254 = format!("{name_253}a");

        assert_eq!(dns_name(&label), Some(label.clone()));
        assert_eq!(dns_name(&"a".repeat(64)), None);
        assert_eq!(dns_name(&name_253), Some(name_253.clone()));
        assert_eq!(dns_name(&name_254), None);
        assert_eq!(
            dns_name("_dmarc.example.com"),
            Some("_dmarc.example.com".into())
        );
        assert_eq!(dns_name("."), None);
    }

    /// Returns the record of the events, each received at its second.
    fn aggregate(events: Vec<(Event, i64)>) -> Record {
        let mut aggregate = Aggregate::default();

        for (event, second) in events {
            aggregate.apply(event, at(second)).unwrap();
        }

        aggregate.to_record()
    }

    #[test]
    fn aggregates_dns_events_per_name() {
        let record = aggregate(vec![
            (dns("example.com", &["1.1.1.1"]), 10),
            (dns("example.com", &["2.2.2.2"]), 20),
        ]);

        let expected = DnsEntry {
            name: "example.com".into(),
            addresses: [ip("1.1.1.1"), ip("2.2.2.2")].into(),
            count: 2,
            first_seen: at(10),
            last_seen: at(20),
        };
        assert_eq!(record.dns, [expected]);
    }

    #[test]
    fn aggregates_connect_events_per_address_and_protocol() {
        let udp = Event::Connect {
            protocol: Protocol::Udp,
            ip: ip("1.1.1.1"),
            port: 443,
        };
        let record = aggregate(vec![
            (connect("1.1.1.1", 443), 30),
            (connect("1.1.1.1", 80), 40),
            (udp, 50),
        ]);

        let tcp = ConnectionEntry {
            ip: ip("1.1.1.1"),
            protocol: Protocol::Tcp,
            ports: [80, 443].into(),
            count: 2,
            first_seen: at(30),
            last_seen: at(40),
        };
        let udp = ConnectionEntry {
            ip: ip("1.1.1.1"),
            protocol: Protocol::Udp,
            ports: [443].into(),
            count: 1,
            first_seen: at(50),
            last_seen: at(50),
        };
        assert_eq!(record.connections, [tcp, udp]);
    }

    #[test]
    fn full_record_drops_new_destinations_but_updates_known_ones() {
        let mut aggregate = Aggregate::default();

        for n in 0..MAX_DESTINATIONS {
            let [_, _, high, low] = u32::try_from(n).unwrap().to_be_bytes();
            let event = connect(&format!("10.0.{high}.{low}"), 443);
            aggregate.apply(event, at(1)).unwrap();
        }

        assert_eq!(aggregate.apply(dns("new.example", &[]), at(2)), Err(Full));
        assert_eq!(aggregate.apply(connect("10.1.0.0", 443), at(2)), Err(Full));
        assert_eq!(aggregate.apply(connect("10.0.0.0", 443), at(2)), Ok(()));

        let record = aggregate.to_record();
        assert_eq!(
            record.dns.len() + record.connections.len(),
            MAX_DESTINATIONS
        );
        assert_eq!(record.connections[0].count, 2);
        assert_eq!(record.connections[0].last_seen, at(2));
    }

    #[test]
    fn entries_keep_at_most_max_values_ports() {
        let mut aggregate = Aggregate::default();

        for port in 1..=200 {
            aggregate.apply(connect("1.1.1.1", port), at(1)).unwrap();
        }

        let record = aggregate.to_record();
        assert_eq!(record.connections[0].ports.len(), MAX_VALUES);
        assert_eq!(record.connections[0].count, 200);
    }

    #[tokio::test]
    async fn corrupt_record_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sb.json");
        std::fs::write(&path, "{not json").unwrap();

        assert_eq!(
            load_record_in_background("sb".into(), path).await,
            Record::default()
        );
        assert_eq!(
            load_record_in_background("sb".into(), dir.path().join("missing.json")).await,
            Record::default()
        );
    }

    #[tokio::test]
    async fn record_of_a_corrupt_file_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("network")).unwrap();
        std::fs::write(dir.path().join("network/sb.json"), "{not json").unwrap();

        let err = netlogs(dir.path()).record("sb").await.unwrap_err();

        assert!(matches!(err, RecordError::Corrupt { .. }), "{err:?}");
        assert!(err.to_string().contains("sb.json is corrupt"), "{err}");
    }

    #[tokio::test]
    async fn record_of_an_unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be can't be read as one, also as root.
        std::fs::create_dir_all(dir.path().join("network/sb.json")).unwrap();

        let err = netlogs(dir.path()).record("sb").await.unwrap_err();

        assert!(matches!(err, RecordError::Read { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn read_line_skips_long_lines_and_keeps_reading() {
        let long = vec![b'x'; MAX_LINE + 10];
        let exact = vec![b'y'; MAX_LINE];
        let input = [&long[..], b"\n", &exact, b"\nlast"].concat();
        let mut reader = BufReader::with_capacity(16, &input[..]);
        let mut buf = vec![];

        assert_eq!(
            read_line(&mut reader, &mut buf).await.unwrap(),
            Line::TooLong
        );
        assert_eq!(
            read_line(&mut reader, &mut buf).await.unwrap(),
            Line::Complete
        );
        assert_eq!(buf, exact);
        assert_eq!(
            read_line(&mut reader, &mut buf).await.unwrap(),
            Line::Complete
        );
        assert_eq!(buf, b"last");
        assert_eq!(read_line(&mut reader, &mut buf).await.unwrap(), Line::End);
    }

    const CONNECT_LINE: &str = r#"{"kind":"connect","protocol":"tcp","ip":"1.1.1.1","port":443}"#;

    /// Returns a registry with its sockets and records in a temp directory.
    fn netlogs(dir: &Path) -> NetLogs {
        NetLogs::new(Some(dir.join("netlog")), Some(dir.join("network")))
    }

    /// Connects to the sandbox's socket like the logger in the guest.
    async fn connect_logger(logs: &NetLogs, sandbox: &str) -> io::Result<UnixStream> {
        UnixStream::connect(logs.socket_path(sandbox).unwrap()).await
    }

    /// Opens `count` connections to the sandbox's socket.
    async fn connect_many(logs: &NetLogs, count: usize) -> Vec<UnixStream> {
        let mut streams = vec![];

        for _ in 0..count {
            streams.push(connect_logger(logs, "sb").await.unwrap());
        }

        streams
    }

    /// Writes the lines, each followed by a newline, to the stream.
    async fn write_lines(stream: &mut UnixStream, lines: &[&str]) {
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
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let first_seen = json["connections"][0]["first_seen"].as_str().unwrap();
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

        assert!(!logs.socket_path("sb").unwrap().exists());
        assert!(!dir.path().join("network/sb.json").exists());
        assert_eq!(logs.record("sb").await.unwrap(), Record::default());
    }

    /// Listens for the sandbox, reads its record and removes it, returning the record.
    async fn listen_and_remove(logs: &NetLogs, sandbox: &str) -> Record {
        logs.listen(sandbox).await;
        let record = logs.record(sandbox).await.unwrap();
        logs.remove(sandbox).await;

        record
    }

    #[tokio::test]
    async fn names_that_leave_the_record_directory_touch_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.json");
        std::fs::write(&outside, "not a record").unwrap();
        std::fs::create_dir(dir.path().join("netlog")).unwrap();
        std::fs::create_dir(dir.path().join("network")).unwrap();
        let logs = netlogs(dir.path());

        let first = listen_and_remove(&logs, "../outside").await;
        let second = listen_and_remove(&logs, "../netlog/outside").await;

        assert_eq!((first, second), (Record::default(), Record::default()));

        assert!(outside.exists());
        assert!(!dir.path().join("outside.sock").exists());
    }
}
