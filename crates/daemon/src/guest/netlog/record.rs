//! The [`Record`] of a sandbox: aggregates its events per destination, caps what it holds,
//! and writes it to and reads it from its file in the network log directory.

use super::parse::{Event, Invalid, parse_line};
use super::{ConnectionEntry, DnsEntry, Protocol, Record, RecordError};
use chrono::{DateTime, Utc};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The most destinations, DNS names and connection targets together, a record holds.
const MAX_DESTINATIONS: usize = 10_000;

/// The most addresses or ports one destination of a record holds.
const MAX_VALUES: usize = 64;

/// How often fbkd logs a warning about invalid lines from one sandbox at most.
const WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// How often fbkd writes a changed record to its file.
pub(super) const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

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
pub(super) enum Source {
    /// Input from the guest, such as an invalid line or too many connections.
    Guest,
    /// A failure on the host, such as a record that can't be written.
    Host,
}

/// The record of one sandbox while fbkd listens for its events.
pub(super) struct Recorder {
    pub(super) sandbox: String,
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
    pub(super) async fn load(sandbox: &str, path: Option<PathBuf>) -> Self {
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
    pub(super) fn record(&self) -> Record {
        self.state().aggregate.to_record()
    }

    /// Records a line from the logger, or drops it with a rate-limited warning when it's
    /// invalid.
    pub(super) fn handle_line(&self, line: &[u8]) {
        match parse_line(line) {
            Ok(event) => self.add(event),
            Err(invalid) => self.drop_invalid(&invalid),
        }
    }

    /// Drops an invalid line with a rate-limited warning.
    pub(super) fn drop_invalid(&self, invalid: &Invalid) {
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
    pub(super) fn warn(&self, source: Source, message: &str) {
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
    pub(super) fn mark_removed(&self) {
        self.state().removed = true;
    }

    /// Writes the record to its file when it changed since the last write.
    pub(super) async fn flush(&self) {
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

/// Reads the record file like [`read_record`], on a blocking thread.
pub(super) async fn read_record_in_background(path: PathBuf) -> Result<Record, RecordError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::netlog::tests::{at, connect, dns, ip, netlogs};

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
}
