//! Port forwarding: listens on host loopback ports and forwards each connection to a port on a
//! sandbox's loopback.
//!
//! [`Forwards`] reconciles the open forwards of a sandbox with a list of [`PortMapping`]s. Each
//! forward listens on `127.0.0.1:<host>`, and on `[::1]:<host>` when the host has IPv6
//! loopback, and hands accepted connections to a [`Connector`]. [`SshConnector`] reaches the
//! guest port through a `direct-tcpip` channel of the sandbox's SSH server, which agentd opens
//! from inside the guest. One SSH session per sandbox carries all channels.
//!
//! [`CallbackForwards`] opens forwards of one sandbox on demand, such as for the OAuth callback
//! port of a URL the sandbox opens on the host, and closes each one after it's idle.

use async_trait::async_trait;
use firebrick_spec::PortMapping;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

mod callback;
mod listener;
mod ssh;

pub use callback::{CALLBACK_IDLE_TIMEOUT, CallbackForwards};
use listener::{Activity, Forward, Route, close, listen};
pub use ssh::{SshClient, SshConnector};

/// Sandbox label that stores the sandbox's port mappings, such as `3000:3000,8080:5173`.
pub const PORTS_LABEL: &str = "firebrick.ports";

/// Returns the value of [`PORTS_LABEL`] for the mappings.
pub fn label_value(ports: &[PortMapping]) -> String {
    ports
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Returns the mappings stored in a [`PORTS_LABEL`] value, skipping entries that aren't valid.
pub fn ports_from_label(value: &str) -> Vec<PortMapping> {
    value
        .split(',')
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| entry.parse().ok())
        .collect()
}

/// A bidirectional byte stream to a port in a sandbox.
pub trait GuestIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> GuestIo for T {}

/// Opens connections to ports in sandboxes.
#[async_trait]
pub trait Connector: Send + Sync + 'static {
    /// Opens a connection to `127.0.0.1:<guest_port>` in the sandbox.
    async fn connect(&self, sandbox: &str, guest_port: u16) -> io::Result<Box<dyn GuestIo>>;

    /// Releases what the connector holds for the sandbox, such as its SSH session.
    async fn forget(&self, sandbox: &str);
}

/// A forward that couldn't be opened, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardFailure {
    /// The mapping that isn't forwarded.
    pub port: PortMapping,
    /// Why the host port couldn't be listened on.
    pub reason: String,
}

/// The forwards of a sandbox after reconciling them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForwardReport {
    /// The forwards that are open, sorted by host port.
    pub open: Vec<PortMapping>,
    /// The forwards that couldn't be opened.
    pub failed: Vec<ForwardFailure>,
}

/// The open forwards of one sandbox by their mapping.
type SandboxForwards = BTreeMap<PortMapping, Forward>;

/// The open forwards of all sandboxes. Dropping it closes them.
pub struct Forwards {
    connector: Arc<dyn Connector>,
    sandboxes: Mutex<HashMap<String, SandboxForwards>>,
}

impl Forwards {
    /// Creates forwards that reach the sandboxes through `connector`.
    pub fn new(connector: impl Connector) -> Self {
        Self {
            connector: Arc::new(connector),
            sandboxes: Mutex::new(HashMap::new()),
        }
    }

    /// Makes the open forwards of the sandbox match `ports`: closes the ones that aren't listed,
    /// opens the listed ones that aren't open yet, and leaves the others and their connections
    /// alone. A host port that can't be listened on is logged and reported, not fatal.
    pub async fn apply(&self, sandbox: &str, ports: &[PortMapping]) -> ForwardReport {
        let mut sandboxes = self.sandboxes.lock().await;
        let mut forwards = sandboxes.remove(sandbox).unwrap_or_default();

        // Close first, so a host port that moves to another guest port can be bound again.
        close_unlisted(&mut forwards, ports, sandbox).await;
        let failed = self.open_missing(&mut forwards, ports, sandbox).await;
        let open = forwards.keys().copied().collect();

        if forwards.is_empty() {
            self.connector.forget(sandbox).await;
        } else {
            sandboxes.insert(sandbox.to_string(), forwards);
        }

        ForwardReport { open, failed }
    }

    /// Opens the listed forwards that aren't open yet, and returns the ones that failed.
    async fn open_missing(
        &self,
        forwards: &mut SandboxForwards,
        ports: &[PortMapping],
        sandbox: &str,
    ) -> Vec<ForwardFailure> {
        let missing: Vec<PortMapping> = ports
            .iter()
            .filter(|port| !forwards.contains_key(port))
            .copied()
            .collect();
        let mut failed = vec![];

        for port in missing {
            let opened = self.open(sandbox, port).await;
            let opened = opened.map(|forward| forwards.insert(port, forward));
            failed.extend(opened.err().map(|err| open_failed(sandbox, port, &err)));
        }

        failed
    }

    /// Whether the sandbox has an open forward of the host port to the guest port.
    pub async fn is_open(&self, sandbox: &str, port: PortMapping) -> bool {
        self.sandboxes
            .lock()
            .await
            .get(sandbox)
            .is_some_and(|forwards| forwards.contains_key(&port))
    }

    /// Closes the forwards of the sandbox. Their host ports can be bound again when it returns.
    pub async fn close(&self, sandbox: &str) {
        let forwards = self.sandboxes.lock().await.remove(sandbox);

        for forward in forwards.into_iter().flat_map(BTreeMap::into_values) {
            close(forward).await;
        }

        self.connector.forget(sandbox).await;
    }

    /// Listens on the host port and forwards its connections to the guest port until the
    /// forward is closed.
    async fn open(&self, sandbox: &str, port: PortMapping) -> io::Result<Forward> {
        listen(Route {
            sandbox: sandbox.to_string(),
            port,
            connector: Arc::clone(&self.connector),
            activity: Arc::new(Activity::new(None)),
        })
        .await
    }
}

/// Closes the forwards that aren't listed in `ports`.
async fn close_unlisted(forwards: &mut SandboxForwards, ports: &[PortMapping], sandbox: &str) {
    let unlisted: Vec<PortMapping> = forwards
        .keys()
        .filter(|port| !ports.contains(port))
        .copied()
        .collect();

    for port in unlisted
        .into_iter()
        .filter_map(|port| forwards.remove_entry(&port))
    {
        close(port.1).await;
        tracing::info!(
            "closed forward of localhost:{} for sandbox {sandbox}",
            port.0.host
        );
    }
}

/// Logs why a forward couldn't be opened and returns the failure.
fn open_failed(sandbox: &str, port: PortMapping, err: &io::Error) -> ForwardFailure {
    tracing::warn!(
        "couldn't forward localhost:{} for sandbox {sandbox}: {err}",
        port.host
    );

    ForwardFailure {
        port,
        reason: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Connects to the guest port on the host's own loopback, standing in for a sandbox.
    struct LocalConnector;

    #[async_trait]
    impl Connector for LocalConnector {
        async fn connect(&self, _sandbox: &str, guest_port: u16) -> io::Result<Box<dyn GuestIo>> {
            Ok(Box::new(
                TcpStream::connect((Ipv4Addr::LOCALHOST, guest_port)).await?,
            ))
        }

        async fn forget(&self, _sandbox: &str) {}
    }

    /// Returns a port that nothing listens on at the moment. A port picked later, also by a
    /// server, may be the same one, so pick a test's ports with one [`free_ports`] call and
    /// start its servers first.
    pub(super) async fn free_port() -> u16 {
        let [port] = free_ports().await;

        port
    }

    /// Returns `N` distinct ports that nothing listens on at the moment. The listeners stay
    /// bound until every port is picked, so the system can't hand out a port twice.
    pub(super) async fn free_ports<const N: usize>() -> [u16; N] {
        let mut listeners = Vec::with_capacity(N);

        for _ in 0..N {
            listeners.push(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        }

        let ports: Vec<u16> = listeners
            .iter()
            .map(|listener| listener.local_addr().unwrap().port())
            .collect();

        ports.try_into().unwrap()
    }

    pub(super) fn mapping(host: u16, guest: u16) -> PortMapping {
        PortMapping { host, guest }
    }

    /// Starts a server on a free port that answers each line with the line in upper case.
    async fn upper_case_server() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(answer_in_upper_case(listener));

        port
    }

    /// Answers each connection to the listener with what it receives in upper case.
    async fn answer_in_upper_case(listener: TcpListener) {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(write_back_in_upper_case(stream));
        }
    }

    /// Writes back what the connection receives in upper case until it closes.
    async fn write_back_in_upper_case(mut stream: TcpStream) {
        let mut buffer = [0; 64];

        while let Ok(count @ 1..) = stream.read(&mut buffer).await {
            buffer[..count].make_ascii_uppercase();
            let _ = stream.write_all(&buffer[..count]).await;
        }
    }

    /// Sends the text over the connection and returns the reply.
    async fn round_trip(stream: &mut TcpStream, text: &str) -> String {
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut reply = vec![0; text.len()];
        stream.read_exact(&mut reply).await.unwrap();

        String::from_utf8(reply).unwrap()
    }

    pub(super) async fn connect(port: u16) -> io::Result<TcpStream> {
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await
    }

    #[test]
    fn label_round_trips_mappings() {
        let ports = [mapping(3000, 3000), mapping(8080, 5173)];

        assert_eq!(label_value(&ports), "3000:3000,8080:5173");
        assert_eq!(ports_from_label(&label_value(&ports)), ports);
        assert!(ports_from_label("").is_empty());
    }

    #[test]
    fn label_skips_invalid_entries() {
        assert_eq!(
            ports_from_label("3000:3000,web,0:80,8080:5173"),
            [mapping(3000, 3000), mapping(8080, 5173)]
        );
    }

    #[tokio::test]
    async fn forwards_bytes_both_ways() {
        let forwards = Forwards::new(LocalConnector);
        let guest = upper_case_server().await;
        let host = free_port().await;

        let report = forwards.apply("dev", &[mapping(host, guest)]).await;
        let mut stream = connect(host).await.unwrap();

        assert_eq!(report.open, [mapping(host, guest)]);
        assert!(report.failed.is_empty());
        assert_eq!(round_trip(&mut stream, "hello").await, "HELLO");
    }

    #[tokio::test]
    async fn apply_opens_added_closes_removed_and_keeps_unchanged_ports() {
        let forwards = Forwards::new(LocalConnector);
        let guest = upper_case_server().await;
        let [kept, removed, added] = free_ports().await;

        forwards
            .apply("dev", &[mapping(kept, guest), mapping(removed, guest)])
            .await;
        let mut open_connection = connect(kept).await.unwrap();
        round_trip(&mut open_connection, "before").await;

        let report = forwards
            .apply("dev", &[mapping(kept, guest), mapping(added, guest)])
            .await;

        let mut expected = vec![mapping(kept, guest), mapping(added, guest)];
        expected.sort();
        assert_eq!(report.open, expected);
        assert_eq!(round_trip(&mut open_connection, "after").await, "AFTER");
        assert!(connect(added).await.is_ok());
        assert!(connect(removed).await.is_err());
        TcpListener::bind((Ipv4Addr::LOCALHOST, removed))
            .await
            .expect("the removed host port should be free again");
    }

    #[tokio::test]
    async fn apply_moves_a_host_port_to_another_guest_port() {
        let forwards = Forwards::new(LocalConnector);
        let new_guest = upper_case_server().await;
        let [host, old_guest] = free_ports().await;

        forwards.apply("dev", &[mapping(host, old_guest)]).await;
        let report = forwards.apply("dev", &[mapping(host, new_guest)]).await;

        assert_eq!(report.open, [mapping(host, new_guest)]);
        assert!(report.failed.is_empty());
        let mut stream = connect(host).await.unwrap();
        assert_eq!(round_trip(&mut stream, "moved").await, "MOVED");
    }

    #[tokio::test]
    async fn busy_host_port_is_reported_and_other_ports_still_open() {
        let forwards = Forwards::new(LocalConnector);
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let busy_port = busy.local_addr().unwrap().port();
        let guest = upper_case_server().await;
        let free = free_port().await;

        let report = forwards
            .apply("dev", &[mapping(busy_port, guest), mapping(free, guest)])
            .await;

        assert_eq!(report.open, [mapping(free, guest)]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].port, mapping(busy_port, guest));
        assert!(!report.failed[0].reason.is_empty());
    }

    #[tokio::test]
    async fn host_port_with_a_listener_on_any_address_is_reported() {
        let forwards = Forwards::new(LocalConnector);
        let busy = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
        let host = busy.local_addr().unwrap().port();
        let guest = upper_case_server().await;

        let report = forwards.apply("dev", &[mapping(host, guest)]).await;

        assert!(report.open.is_empty());
        assert_eq!(report.failed.len(), 1);
    }

    #[tokio::test]
    async fn host_port_with_open_connections_of_a_closed_forward_can_be_reopened() {
        let forwards = Forwards::new(LocalConnector);
        let guest = upper_case_server().await;
        let host = free_port().await;
        forwards.apply("dev", &[mapping(host, guest)]).await;
        let mut stream = connect(host).await.unwrap();
        round_trip(&mut stream, "hello").await;

        forwards.close("dev").await;
        drop(stream);
        let report = forwards.apply("dev", &[mapping(host, guest)]).await;

        assert_eq!(report.open, [mapping(host, guest)]);
    }

    #[tokio::test]
    async fn host_port_of_another_sandbox_is_reported() {
        let forwards = Forwards::new(LocalConnector);
        let guest = upper_case_server().await;
        let host = free_port().await;

        forwards.apply("first", &[mapping(host, guest)]).await;
        let report = forwards.apply("second", &[mapping(host, guest)]).await;

        assert!(report.open.is_empty());
        assert_eq!(report.failed.len(), 1);
    }

    #[tokio::test]
    async fn failed_port_is_retried_on_the_next_apply() {
        let forwards = Forwards::new(LocalConnector);
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let host = busy.local_addr().unwrap().port();
        let guest = upper_case_server().await;

        forwards.apply("dev", &[mapping(host, guest)]).await;
        drop(busy);
        let report = forwards.apply("dev", &[mapping(host, guest)]).await;

        assert_eq!(report.open, [mapping(host, guest)]);
    }

    #[tokio::test]
    async fn connection_without_guest_listener_closes_and_listener_stays_up() {
        let forwards = Forwards::new(LocalConnector);
        let [host, guest] = free_ports().await;
        forwards.apply("dev", &[mapping(host, guest)]).await;

        let mut stream = connect(host).await.unwrap();
        let mut buffer = [0; 1];
        let read = stream.read(&mut buffer).await;
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "the connection should close: {read:?}"
        );

        // A server starts listening on the guest port.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, guest))
            .await
            .unwrap();
        let mut stream = connect(host).await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        let (mut accepted, _) = listener.accept().await.unwrap();
        let mut received = [0; 4];
        accepted.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"ping");
    }

    #[tokio::test]
    async fn close_frees_the_host_ports() {
        let forwards = Forwards::new(LocalConnector);
        let guest = upper_case_server().await;
        let host = free_port().await;
        forwards.apply("dev", &[mapping(host, guest)]).await;

        forwards.close("dev").await;

        assert!(connect(host).await.is_err());
        TcpListener::bind((Ipv4Addr::LOCALHOST, host))
            .await
            .expect("the host port should be free again");
        assert!(forwards.apply("dev", &[]).await.open.is_empty());
    }

    #[tokio::test]
    async fn dropping_the_forwards_closes_them() {
        let forwards = Forwards::new(LocalConnector);
        let guest = upper_case_server().await;
        let host = free_port().await;
        forwards.apply("dev", &[mapping(host, guest)]).await;

        drop(forwards);
        tokio::task::yield_now().await;

        assert!(connect(host).await.is_err());
    }
}
