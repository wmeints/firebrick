//! Records the domains and IP addresses that sandboxes contact.
//!
//! microsandbox doesn't report the DNS queries or connections of a sandbox, so a logger in the
//! `firebrick-base` image watches the guest's network interface and writes one JSON event per
//! line to vsock port [`GUEST_PORT`] on the host. Each sandbox gets a stream route from that port
//! to its own unix socket in [`firebrick_utils::netlog_socket_dir`], where fbkd listens. fbkd
//! treats every line as untrusted input: the logger runs inside the VM, so the agent can stop it
//! or send false events. It aggregates the valid events per destination into a record file in
//! [`firebrick_utils::network_log_dir`]. The record is a diagnostic aid, not an audit trail.

use chrono::{DateTime, Utc};
use firebrick_utils::strip_control;
use microsandbox::sandbox::{SandboxBuilder, SandboxSpec, VsockSocketType};
use serde::{Deserialize, Serialize};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::DirBuilder;
use std::io;
use std::net::Ipv4Addr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::{AbortHandle, JoinSet};

/// Guest vsock port the logger in the guest writes its events to, on host CID 2.
pub const GUEST_PORT: u32 = 52000;

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
                let new = || DnsEntry::new(name.clone(), now);
                let entry = upsert(&mut self.dns, name.clone(), full, new)?;
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
    /// When fbkd last logged a warning about this sandbox.
    last_warning: Option<Instant>,
    /// Whether fbkd logged that the record is full.
    warned_full: bool,
}

impl Recorder {
    /// Loads the record of the sandbox from `path`, or starts an empty one when there's no file
    /// or it's corrupt. Without a path, the record is kept in memory only.
    fn load(sandbox: &str, path: Option<PathBuf>) -> Self {
        let record = path
            .as_deref()
            .map(|path| load_record(sandbox, path))
            .unwrap_or_default();

        Self {
            sandbox: sandbox.to_string(),
            path,
            state: Mutex::new(RecorderState {
                aggregate: Aggregate::from_record(record),
                dirty: false,
                last_warning: None,
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
        self.warn(&format!("dropped an invalid event: {invalid}"));
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

    /// Logs a warning about the sandbox, at most once per [`WARNING_INTERVAL`].
    fn warn(&self, message: &str) {
        let mut state = self.state();
        let now = Instant::now();

        if state
            .last_warning
            .is_some_and(|last| now.duration_since(last) < WARNING_INTERVAL)
        {
            return;
        }

        state.last_warning = Some(now);
        tracing::warn!("network log of sandbox {}: {message}", self.sandbox);
    }

    /// Writes the record to its file when it changed since the last write.
    async fn flush(self: &Arc<Self>) {
        let (Some(path), Some(content)) = (self.path.clone(), self.take_changes()) else {
            return;
        };
        let written = match content {
            Ok(content) => write_in_background(path, content).await,
            Err(err) => Err(io::Error::other(err)),
        };

        if let Err(err) = written {
            self.warn(&format!("failed to write the record: {err}"));
        }
    }

    /// Returns the record as JSON when it changed since the last call.
    fn take_changes(&self) -> Option<serde_json::Result<String>> {
        let mut state = self.state();
        let dirty = std::mem::replace(&mut state.dirty, false);

        dirty.then(|| serde_json::to_string(&state.aggregate.to_record()))
    }
}

/// Writes the private file on a blocking thread, so a large record doesn't stall the runtime.
async fn write_in_background(path: PathBuf, content: String) -> io::Result<()> {
    tokio::task::spawn_blocking(move || firebrick_utils::write_private(&path, &content))
        .await
        .unwrap_or_else(|err| Err(io::Error::other(err)))
}

/// Reads the record file, or returns an empty record when there's none or it's corrupt.
fn load_record(sandbox: &str, path: &Path) -> Record {
    let content = match std::fs::read(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Record::default(),
        Err(err) => {
            tracing::warn!("failed to read the network record of sandbox {sandbox}: {err}");
            return Record::default();
        }
    };

    serde_json::from_slice(&content).unwrap_or_else(|err| {
        tracing::warn!(
            "the network record of sandbox {sandbox} is corrupt; starting a new one: {err}"
        );
        Record::default()
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
                recorder.warn(&format!("failed to read events: {err}"));
                return;
            }
        }
    }
}

/// Accepts connections from the logger on the listener and records their events, writing the
/// record to its file every [`FLUSH_INTERVAL`] while it changes. Runs until it's aborted, which
/// also stops its connections.
async fn serve(listener: UnixListener, recorder: Arc<Recorder>) {
    let mut connections = JoinSet::new();
    let mut flush = tokio::time::interval(FLUSH_INTERVAL);

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(read_events(stream, Arc::clone(&recorder)));
                }
                Err(err) => {
                    recorder.warn(&format!("failed to accept a connection: {err}"));
                    tokio::time::sleep(FLUSH_INTERVAL).await;
                }
            },
            _ = flush.tick() => recorder.flush().await,
            Some(_) = connections.join_next() => {}
        }
    }
}

/// A sandbox fbkd listens for.
struct Listening {
    recorder: Arc<Recorder>,
    task: AbortHandle,
}

/// Listens for the network events of sandboxes and keeps their records.
pub struct NetLogs {
    socket_dir: PathBuf,
    record_dir: Option<PathBuf>,
    listening: Mutex<HashMap<String, Listening>>,
}

impl NetLogs {
    /// Creates a registry with the sockets in `socket_dir` and the records in `record_dir`.
    /// Without a record directory, records are kept in memory only.
    pub fn new(socket_dir: PathBuf, record_dir: Option<PathBuf>) -> Self {
        Self {
            socket_dir,
            record_dir,
            listening: Mutex::default(),
        }
    }

    /// Creates a registry in the directories the environment selects, see
    /// [`firebrick_utils::netlog_socket_dir`] and [`firebrick_utils::network_log_dir`].
    pub fn from_env() -> Self {
        Self::new(
            firebrick_utils::netlog_socket_dir(),
            firebrick_utils::network_log_dir(),
        )
    }

    /// Locks the registry. It stays consistent when a thread panics while holding it, so a
    /// poisoned lock is safe to use.
    fn listening(&self) -> MutexGuard<'_, HashMap<String, Listening>> {
        self.listening
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns the socket of the sandbox, or `None` when its path is too long for a unix socket.
    pub fn socket_path(&self, sandbox: &str) -> Option<PathBuf> {
        let path = self.socket_dir.join(format!("{sandbox}.sock"));

        std::os::unix::net::SocketAddr::from_pathname(&path)
            .is_ok()
            .then_some(path)
    }

    /// Returns the record file of the sandbox, if there's a record directory.
    fn record_path(&self, sandbox: &str) -> Option<PathBuf> {
        self.record_dir
            .as_ref()
            .map(|dir| dir.join(format!("{sandbox}.json")))
    }

    /// Adds the route from [`GUEST_PORT`] to the sandbox's socket to the builder. Leaves the
    /// builder alone, with a warning, when the socket path is too long, because microsandbox
    /// would then refuse to boot the sandbox.
    pub fn add_route(&self, builder: SandboxBuilder, sandbox: &str) -> SandboxBuilder {
        match self.socket_path(sandbox) {
            Some(path) => builder.vsock(path, GUEST_PORT),
            None => {
                tracing::warn!(
                    "the network log socket of sandbox {sandbox} would be too long; it gets no \
                     network log"
                );
                builder
            }
        }
    }

    /// Whether the sandbox has a stream route from [`GUEST_PORT`] to its socket.
    pub fn has_route(&self, sandbox: &str, spec: &SandboxSpec) -> bool {
        let Some(path) = self.socket_path(sandbox) else {
            return false;
        };

        spec.vsock.routes.iter().any(|route| {
            route.port == GUEST_PORT
                && route.socket_type == VsockSocketType::Stream
                && route.host_socket == path
        })
    }

    /// Listens on the sandbox's socket unless fbkd already does, loading its record. Removes a
    /// stale socket file first, but leaves one alone that another process listens on. Logs an
    /// error when the socket can't be bound: network logging never stops a sandbox from
    /// starting.
    pub async fn listen(&self, sandbox: &str) {
        if self.listening().contains_key(sandbox) {
            return;
        }

        let listener = match self.bind(sandbox).await {
            Ok(listener) => listener,
            Err(err) => {
                tracing::error!("failed to listen for the network log of sandbox {sandbox}: {err}");
                return;
            }
        };
        let recorder = Arc::new(Recorder::load(sandbox, self.record_path(sandbox)));
        let task = tokio::spawn(serve(listener, Arc::clone(&recorder))).abort_handle();

        // Another task may have started listening while this one bound the socket. Its listener
        // lost the path to this one, so this one takes its place.
        let replaced = self
            .listening()
            .insert(sandbox.to_string(), Listening { recorder, task });

        if let Some(replaced) = replaced {
            replaced.task.abort();
        }
    }

    /// Binds the sandbox's socket, removing a stale socket file first.
    async fn bind(&self, sandbox: &str) -> io::Result<UnixListener> {
        let path = self
            .socket_path(sandbox)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path is too long"))?;

        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.socket_dir)?;

        if UnixStream::connect(&path).await.is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another process listens on {}", path.display()),
            ));
        }

        remove_if_exists(&path)?;

        UnixListener::bind(&path)
    }

    /// Returns the network activity recorded for the sandbox: the live record while fbkd
    /// listens for it, otherwise its record file, which is empty when there's none.
    pub fn record(&self, sandbox: &str) -> Record {
        if let Some(listening) = self.listening().get(sandbox) {
            return listening.recorder.record();
        }

        self.record_path(sandbox)
            .map(|path| load_record(sandbox, &path))
            .unwrap_or_default()
    }

    /// Stops listening for the sandbox and deletes its socket and record. Logs a warning when
    /// a file can't be deleted.
    pub fn remove(&self, sandbox: &str) {
        if let Some(listening) = self.listening().remove(sandbox) {
            listening.task.abort();
        }

        let files = [self.socket_path(sandbox), self.record_path(sandbox)];

        files.iter().flatten().for_each(|path| remove_logged(path));
    }
}

impl Drop for NetLogs {
    fn drop(&mut self) {
        for listening in self.listening().values() {
            listening.task.abort();
        }
    }
}

/// Removes the file, logging a warning when that fails.
fn remove_logged(path: &Path) {
    if let Err(err) = remove_if_exists(path) {
        tracing::warn!("failed to remove {}: {err}", path.display());
    }
}

/// Removes the file, succeeding when it doesn't exist.
fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
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
    fn rejects_invalid_events() {
        let lines: [&[u8]; 9] = [
            b"not json",
            br#"{"kind":"http","url":"https://example.com"}"#,
            br#"{"kind":"dns","name":"bad name","addresses":[]}"#,
            br#"{"kind":"dns","name":"","addresses":[]}"#,
            br#"{"kind":"dns","name":"a..b","addresses":[]}"#,
            br#"{"kind":"dns","name":"example.com","addresses":["::1"]}"#,
            br#"{"kind":"connect","protocol":"tcp","ip":"300.1.1.1","port":443}"#,
            br#"{"kind":"connect","protocol":"tcp","ip":"1.1.1.1","port":70000}"#,
            br#"{"kind":"connect","protocol":"icmp","ip":"1.1.1.1","port":1}"#,
        ];

        for line in lines {
            assert!(
                parse_line(line).is_err(),
                "{}",
                String::from_utf8_lossy(line)
            );
        }

        assert_eq!(
            parse_line(br#"{"kind":"connect","protocol":"tcp","ip":"1.1.1.1","port":0}"#),
            Err(Invalid::Port)
        );
        assert_eq!(parse_line(&[b' '; MAX_LINE + 1]), Err(Invalid::TooLong));
    }

    #[test]
    fn dns_name_checks_label_and_name_lengths() {
        let label = "a".repeat(63);
        let long_name = [label.as_str(); 4].join(".");

        assert_eq!(dns_name(&label), Some(label.clone()));
        assert_eq!(dns_name(&"a".repeat(64)), None);
        assert_eq!(long_name.len(), 255);
        assert_eq!(dns_name(&long_name), None);
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
        let udp = ConnectionEntry::new(ip("1.1.1.1"), Protocol::Udp, at(50));
        let udp = ConnectionEntry {
            ports: [443].into(),
            count: 1,
            ..udp
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

    #[test]
    fn record_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sb.json");
        let mut aggregate = Aggregate::default();
        aggregate
            .apply(dns("example.com", &["1.1.1.1"]), at(1))
            .unwrap();
        aggregate.apply(connect("1.1.1.1", 443), at(1)).unwrap();
        let record = aggregate.to_record();
        let json = serde_json::to_string(&record).unwrap();
        firebrick_utils::write_private(&path, &json).unwrap();

        assert_eq!(load_record("sb", &path), record);
        assert!(
            json.contains(r#""first_seen":"1970-01-01T00:00:01Z""#),
            "{json}"
        );
    }

    #[test]
    fn corrupt_record_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sb.json");
        std::fs::write(&path, "{not json").unwrap();

        assert_eq!(load_record("sb", &path), Record::default());
        assert_eq!(
            load_record("sb", &dir.path().join("missing.json")),
            Record::default()
        );
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

    /// Returns a registry with its sockets and records in a temp directory.
    fn netlogs(dir: &Path) -> NetLogs {
        NetLogs::new(dir.join("netlog"), Some(dir.join("network")))
    }

    /// Waits up to 5 seconds until `check` returns a value.
    async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
        let mut value = check();
        let mut attempts = 0;

        while value.is_none() && attempts < 500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            value = check();
            attempts += 1;
        }

        value.unwrap_or_else(|| panic!("timed out waiting for {what}"))
    }

    /// Waits until the record of the sandbox has the expected number of destinations.
    async fn wait_for_destinations(logs: &NetLogs, sandbox: &str, expected: usize) -> Record {
        eventually("the destinations", || {
            let record = logs.record(sandbox);
            (record.dns.len() + record.connections.len() == expected).then_some(record)
        })
        .await
    }

    #[tokio::test]
    async fn records_valid_lines_and_keeps_the_connection_after_invalid_ones() {
        let dir = tempfile::tempdir().unwrap();
        let logs = netlogs(dir.path());
        logs.listen("sb").await;
        let mut stream = UnixStream::connect(logs.socket_path("sb").unwrap())
            .await
            .unwrap();

        let lines = [
            r#"{"kind":"dns","name":"example.com","addresses":["1.1.1.1"]}"#.to_string(),
            "garbage".to_string(),
            "x".repeat(MAX_LINE + 1),
            r#"{"kind":"connect","protocol":"tcp","ip":"1.1.1.1","port":443}"#.to_string(),
        ];
        stream
            .write_all(format!("{}\n", lines.join("\n")).as_bytes())
            .await
            .unwrap();

        let record = wait_for_destinations(&logs, "sb", 2).await;
        assert_eq!(record.dns[0].name, "example.com");
        assert_eq!(record.connections[0].ports, [443].into());
    }

    #[tokio::test]
    async fn writes_the_record_privately_and_loads_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let logs = netlogs(dir.path());
        logs.listen("sb").await;
        let mut stream = UnixStream::connect(logs.socket_path("sb").unwrap())
            .await
            .unwrap();
        stream
            .write_all(
                b"{\"kind\":\"connect\",\"protocol\":\"tcp\",\"ip\":\"1.1.1.1\",\"port\":443}\n",
            )
            .await
            .unwrap();
        let record = wait_for_destinations(&logs, "sb", 1).await;
        let path = dir.path().join("network/sb.json");

        eventually("the record file", || path.exists().then_some(())).await;
        drop(logs);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(netlogs(dir.path()).record("sb"), record);
    }

    #[tokio::test]
    async fn listen_replaces_a_stale_socket_but_not_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let first = netlogs(dir.path());
        first.listen("sb").await;
        let second = netlogs(dir.path());

        second.listen("sb").await;
        assert!(second.listening().is_empty(), "took a live socket");

        drop(first);
        tokio::task::yield_now().await;
        second.listen("sb").await;
        assert!(second.listening().contains_key("sb"));
        UnixStream::connect(second.socket_path("sb").unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn listen_logs_and_continues_when_the_socket_cant_be_bound() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("netlog"), "not a directory").unwrap();
        let logs = netlogs(dir.path());

        logs.listen("sb").await;

        assert!(logs.listening().is_empty());
    }

    #[tokio::test]
    async fn remove_deletes_the_socket_and_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let logs = netlogs(dir.path());
        logs.listen("sb").await;
        let record = dir.path().join("network/sb.json");
        firebrick_utils::write_private(&record, "{}").unwrap();

        logs.remove("sb");

        assert!(!logs.socket_path("sb").unwrap().exists());
        assert!(!record.exists());
        assert!(logs.listening().is_empty());
    }

    #[test]
    fn socket_path_is_none_when_too_long() {
        let logs = NetLogs::new(PathBuf::from("/run/user/1000/firebrick/netlog"), None);

        assert!(logs.socket_path("short").is_some());
        assert!(logs.socket_path(&"a".repeat(100)).is_none());
    }

    #[tokio::test]
    async fn add_route_adds_a_stream_route_to_the_socket() {
        let logs = NetLogs::new(PathBuf::from("/run/firebrick/netlog"), None);
        let builder = logs.add_route(
            microsandbox::Sandbox::builder("sb").image("ubuntu:26.04"),
            "sb",
        );
        let spec = builder.build().await.unwrap().spec;

        assert!(logs.has_route("sb", &spec));
        assert!(!logs.has_route("other", &spec));
    }
}
