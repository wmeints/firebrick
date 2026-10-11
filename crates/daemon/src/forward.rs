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
use microsandbox::Sandbox;
use russh::client::{self, Handle};
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg, PublicKeyBase64};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

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

/// The listeners of one forward. Dropping it closes them and their connections.
struct Forward(JoinSet<()>);

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

/// Where a forward sends its connections, and what tracks whether it's idle.
#[derive(Clone)]
struct Route {
    sandbox: String,
    port: PortMapping,
    connector: Arc<dyn Connector>,
    activity: Arc<Activity>,
}

/// Listens on the route's host port and forwards its connections to the guest port, until the
/// forward is dropped or the route's activity says it's idle.
async fn listen(route: Route) -> io::Result<Forward> {
    let mut listeners = JoinSet::new();
    let Route { sandbox, port, .. } = &route;

    for listener in bind_loopback(port.host).await? {
        listeners.spawn(accept_loop(listener, route.clone()));
    }

    tracing::info!(
        "forwarding localhost:{} to port {} of sandbox {sandbox}",
        port.host,
        port.guest
    );

    Ok(Forward(listeners))
}

/// How long a callback forward stays open without open or new connections, or new URLs for
/// its port.
pub const CALLBACK_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Forwards of host ports to the same ports in one sandbox, opened on demand. Each one closes
/// after it's idle for the timeout. Dropping it closes them all.
pub struct CallbackForwards {
    sandbox: String,
    connector: Arc<dyn Connector>,
    configured: Arc<Forwards>,
    idle_timeout: Duration,
    open: HashMap<u16, CallbackForward>,
}

/// An open callback forward and what keeps it from being idle.
struct CallbackForward {
    forward: Forward,
    activity: Arc<Activity>,
}

impl CallbackForwards {
    /// Creates the callback forwards of the sandbox, reached through `connector`, that close
    /// after `idle_timeout` without connections. A port that `configured` already forwards to
    /// the same port in the sandbox needs no callback forward.
    pub fn new(
        sandbox: &str,
        connector: Arc<dyn Connector>,
        configured: Arc<Forwards>,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            sandbox: sandbox.to_string(),
            connector,
            configured,
            idle_timeout,
            open: HashMap::new(),
        }
    }

    /// Forwards `localhost:<port>` on the host to the same port in the sandbox, reusing the
    /// open forward of the port and resetting its idle timer, or the sandbox's configured
    /// forward of the port. Logs a warning and fails when the host port can't be listened on.
    pub async fn ensure(&mut self, port: u16) -> io::Result<()> {
        let mapping = PortMapping {
            host: port,
            guest: port,
        };

        if self.configured.is_open(&self.sandbox, mapping).await {
            return Ok(());
        }

        if self
            .open
            .get(&port)
            .is_some_and(|open| open.activity.touch())
        {
            return Ok(());
        }

        // Wait until the listeners of an idle forward are closed, so the port can be bound again.
        if let Some(idle) = self.open.remove(&port) {
            close(idle.forward).await;
        }

        let opened = self.open_new(port).await?;
        self.open.insert(port, opened);

        Ok(())
    }

    /// Listens on the host port and forwards its connections to the same guest port until it's
    /// idle. Logs a warning when it can't listen.
    async fn open_new(&self, port: u16) -> io::Result<CallbackForward> {
        let mapping = PortMapping {
            host: port,
            guest: port,
        };
        let activity = Arc::new(Activity::new(Some(self.idle_timeout)));
        let route = Route {
            sandbox: self.sandbox.clone(),
            port: mapping,
            connector: Arc::clone(&self.connector),
            activity: Arc::clone(&activity),
        };

        match listen(route).await {
            Ok(forward) => Ok(CallbackForward { forward, activity }),
            Err(err) => {
                open_failed(&self.sandbox, mapping, &err);
                Err(err)
            }
        }
    }
}

/// When a forward was last used and how many connections it has open, to close it after it's
/// idle.
struct Activity {
    idle_timeout: Option<Duration>,
    state: std::sync::Mutex<ActivityState>,
}

/// The mutable part of [`Activity`].
struct ActivityState {
    last_used: Instant,
    connections: usize,
    idle: bool,
}

impl Activity {
    /// Starts tracking a forward that is idle after `idle_timeout`, or never when it's `None`.
    fn new(idle_timeout: Option<Duration>) -> Self {
        Self {
            idle_timeout,
            state: std::sync::Mutex::new(ActivityState {
                last_used: Instant::now(),
                connections: 0,
                idle: false,
            }),
        }
    }

    /// Locks the state. It stays consistent when a holder panics, so a poisoned lock is used as
    /// is.
    fn state(&self) -> std::sync::MutexGuard<'_, ActivityState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resets the idle timer and returns `true`, or returns `false` when the forward is idle
    /// and closed already.
    fn touch(&self) -> bool {
        let mut state = self.state();
        state.last_used = Instant::now();

        !state.idle
    }

    /// Counts a new connection until the returned guard is dropped.
    fn connected(self: &Arc<Self>) -> ConnectionGuard {
        self.state().connections += 1;

        ConnectionGuard(Arc::clone(self))
    }

    /// Returns once the forward is idle: it has no open connections and wasn't used within the
    /// timeout. Never returns without a timeout.
    async fn idle(&self) {
        let Some(timeout) = self.idle_timeout else {
            return std::future::pending().await;
        };

        while let Some(wait) = self.time_until_idle(timeout) {
            tokio::time::sleep(wait).await;
        }
    }

    /// Returns how long to wait before checking again, or `None` once the forward is idle,
    /// which marks it so.
    fn time_until_idle(&self, timeout: Duration) -> Option<Duration> {
        let mut state = self.state();
        let unused = timeout.saturating_sub(state.last_used.elapsed());

        if state.idle || (state.connections == 0 && unused.is_zero()) {
            state.idle = true;
            return None;
        }

        // A connection that closes resets the timer, so wait a full timeout while one is open.
        Some(if state.connections > 0 {
            timeout
        } else {
            unused
        })
    }
}

/// Counts an open connection of a forward. Dropping it resets the forward's idle timer.
struct ConnectionGuard(Arc<Activity>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.connections = state.connections.saturating_sub(1);
        state.last_used = Instant::now();
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

/// Stops the listeners of a forward and waits until they are closed.
async fn close(mut forward: Forward) {
    forward.0.shutdown().await;
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

/// Listens on the port at `127.0.0.1`, and at `[::1]` when the host has IPv6 loopback. Fails
/// when the port is in use on any address.
async fn bind_loopback(port: u16) -> io::Result<Vec<TcpListener>> {
    ensure_not_listening_on_any_address(port)?;
    let ipv4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;

    match TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
        Ok(ipv6) => Ok(vec![ipv4, ipv6]),
        Err(err) if is_taken(&err) => Err(err),
        // The host has no IPv6 loopback.
        Err(_) => Ok(vec![ipv4]),
    }
}

/// Fails when another socket listens on the port at the IPv4 or IPv6 wildcard address.
/// `TcpListener::bind` sets `SO_REUSEADDR`, which on macOS and the BSDs lets a loopback bind
/// succeed next to such a listener, so `localhost` connections would silently go to the sandbox
/// instead of to the host's server. Binding the wildcard address with the same option fails then,
/// while it still succeeds next to connections of an earlier forward in `TIME_WAIT`.
fn ensure_not_listening_on_any_address(port: u16) -> io::Result<()> {
    for address in [
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)),
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
    ] {
        // The socket is closed right away; a host without IPv6 fails with another error.
        probe_bind(address).or_else(|err| if is_taken(&err) { Err(err) } else { Ok(()) })?;
    }

    Ok(())
}

/// Binds a socket to the address with `SO_REUSEADDR`, without listening, and closes it.
fn probe_bind(address: SocketAddr) -> io::Result<()> {
    let socket = match address {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.set_reuseaddr(true)?;

    socket.bind(address)
}

/// Whether binding failed because the port is in use or needs privileges, rather than because
/// the address family isn't available.
fn is_taken(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

/// Accepts connections on the listener and forwards each one until the forward is closed,
/// which also closes its open connections, or until it's idle.
async fn accept_loop(listener: TcpListener, route: Route) {
    let Route {
        sandbox,
        port,
        connector,
        activity,
    } = route;
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let guard = activity.connected();
                    let relayed = relay(stream, sandbox.clone(), port.guest, Arc::clone(&connector));
                    connections.spawn(async move {
                        relayed.await;
                        drop(guard);
                    });
                }
                Err(err) => accept_failed(port.host, &err).await,
            },
            Some(_) = connections.join_next() => {}
            () = activity.idle() => {
                tracing::debug!("closed idle forward of localhost:{} for sandbox {sandbox}", port.host);
                return;
            }
        }
    }
}

/// How long the accept loop waits after a failed accept. Errors such as running out of file
/// descriptors repeat right away until a connection closes, and would make the loop spin.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Logs why accepting a connection failed and waits before the next accept.
async fn accept_failed(host_port: u16, err: &io::Error) {
    tracing::debug!("couldn't accept on localhost:{host_port}: {err}");
    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
}

/// Connects the host connection to the guest port and copies bytes both ways until both sides
/// are done. Closes the host connection when nothing listens on the guest port.
async fn relay(
    mut host: TcpStream,
    sandbox: String,
    guest_port: u16,
    connector: Arc<dyn Connector>,
) {
    let mut guest = match connector.connect(&sandbox, guest_port).await {
        Ok(guest) => guest,
        Err(err) => {
            tracing::debug!("couldn't connect to port {guest_port} of sandbox {sandbox}: {err}");
            return;
        }
    };

    if let Err(err) = tokio::io::copy_bidirectional(&mut host, &mut guest).await {
        tracing::debug!("forwarded connection to sandbox {sandbox} ended: {err}");
    }
}

/// Receive window of the SSH client. It is small, so the in-memory pipe below can always hold
/// a full window in each direction and neither side blocks on a full pipe.
const SSH_WINDOW_SIZE: u32 = 256 * 1024;

/// Buffer size of the in-memory pipe between the SSH client and server, per direction. It
/// holds the server's default window of 2 MiB plus the client's window and framing overhead.
const SSH_PIPE_SIZE: usize = 4 * 1024 * 1024;

/// Opens connections through `direct-tcpip` channels of the sandboxes' SSH servers, with one
/// SSH session per sandbox that is opened on first use and reopened when it has closed.
#[derive(Default)]
pub struct SshConnector {
    sessions: Mutex<HashMap<String, Arc<SessionSlot>>>,
}

/// The SSH session of one sandbox, if one was opened. Locked while the session opens, so
/// connections to the sandbox wait for it without blocking other sandboxes.
type SessionSlot = Mutex<Option<Arc<Handle<SshClient>>>>;

#[async_trait]
impl Connector for SshConnector {
    async fn connect(&self, sandbox: &str, guest_port: u16) -> io::Result<Box<dyn GuestIo>> {
        let session = self.session(sandbox).await?;
        let channel = session
            .channel_open_direct_tcpip("127.0.0.1", u32::from(guest_port), "127.0.0.1", 0)
            .await
            .map_err(io::Error::other)?;

        Ok(Box::new(channel.into_stream()))
    }

    async fn forget(&self, sandbox: &str) {
        self.sessions.lock().await.remove(sandbox);
    }
}

impl SshConnector {
    /// Returns the open SSH session to the sandbox, opening a new one when needed.
    async fn session(&self, sandbox: &str) -> io::Result<Arc<Handle<SshClient>>> {
        let slot = Arc::clone(
            self.sessions
                .lock()
                .await
                .entry(sandbox.to_string())
                .or_default(),
        );
        let mut slot = slot.lock().await;

        if let Some(session) = slot.as_ref().filter(|session| !session.is_closed()) {
            return Ok(Arc::clone(session));
        }

        let session = Arc::new(open_session(sandbox).await.map_err(|err| {
            tracing::warn!("couldn't open SSH session to sandbox {sandbox}: {err}");
            io::Error::other(err)
        })?);
        *slot = Some(Arc::clone(&session));

        Ok(session)
    }
}

/// Serves the sandbox's SSH server over an in-memory pipe and logs in to it. The keys are
/// generated for this session only, because the pipe never leaves the daemon.
async fn open_session(sandbox: &str) -> anyhow::Result<Handle<SshClient>> {
    let client_key = PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519)?;
    let client_end = serve_ssh(sandbox, &client_key).await?;

    let config = client::Config {
        window_size: SSH_WINDOW_SIZE,
        ..Default::default()
    };
    let mut session = client::connect_stream(Arc::new(config), client_end, SshClient).await?;
    let auth = session
        .authenticate_publickey(
            "root",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await?;

    anyhow::ensure!(auth.success(), "SSH public-key authentication failed");

    Ok(session)
}

/// Serves the sandbox's SSH server on one end of an in-memory pipe, accepting `client_key`, and
/// returns the other end.
async fn serve_ssh(sandbox: &str, client_key: &PrivateKey) -> anyhow::Result<DuplexStream> {
    let sb = Sandbox::get(sandbox).await?.connect().await?;
    let host_key = PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519)?;
    let authorized_key = client_key.public_key().public_key_base64();

    // Configured forwards stay open while the sandbox runs, however long they are idle.
    let server = sb
        .ssh()
        .server_with(|opts| {
            opts.host_key(host_key)
                .authorized_key(authorized_key)
                .disable_inactivity_timeout()
        })
        .await?;

    let (client_end, server_end) = tokio::io::duplex(SSH_PIPE_SIZE);
    let name = sandbox.to_string();
    tokio::spawn(async move {
        if let Err(err) = server.serve(server_end).await {
            tracing::debug!("SSH session for forwards of sandbox {name} ended: {err}");
        }
    });

    Ok(client_end)
}

/// The SSH client of the forwards.
pub struct SshClient;

impl client::Handler for SshClient {
    type Error = russh::Error;

    /// Accepts the server's key: the server runs in this process with a key generated for the
    /// session, and the pipe to it never leaves the daemon.
    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    async fn free_port() -> u16 {
        let [port] = free_ports().await;

        port
    }

    /// Returns `N` distinct ports that nothing listens on at the moment. The listeners stay
    /// bound until every port is picked, so the system can't hand out a port twice.
    async fn free_ports<const N: usize>() -> [u16; N] {
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

    fn mapping(host: u16, guest: u16) -> PortMapping {
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

    async fn connect(port: u16) -> io::Result<TcpStream> {
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

    /// Idle timeout of the callback forwards in the tests.
    const IDLE: Duration = Duration::from_millis(300);

    /// Refuses every connection, standing in for a guest port that nothing listens on. The
    /// callback forwards use the host port as guest port, which [`LocalConnector`] would
    /// connect back to the forward itself.
    struct RefusingConnector;

    #[async_trait]
    impl Connector for RefusingConnector {
        async fn connect(&self, _sandbox: &str, _guest_port: u16) -> io::Result<Box<dyn GuestIo>> {
            Err(io::ErrorKind::ConnectionRefused.into())
        }

        async fn forget(&self, _sandbox: &str) {}
    }

    /// Accepts every connection and keeps the guest's end open, so the connection stays open
    /// until the host closes it.
    #[derive(Default)]
    struct HoldingConnector {
        guest_ends: Mutex<Vec<DuplexStream>>,
    }

    #[async_trait]
    impl Connector for HoldingConnector {
        async fn connect(&self, _sandbox: &str, _guest_port: u16) -> io::Result<Box<dyn GuestIo>> {
            let (host_end, guest_end) = tokio::io::duplex(64);
            self.guest_ends.lock().await.push(guest_end);

            Ok(Box::new(host_end))
        }

        async fn forget(&self, _sandbox: &str) {}
    }

    fn callback_forwards() -> CallbackForwards {
        let configured = Arc::new(Forwards::new(RefusingConnector));

        CallbackForwards::new("dev", Arc::new(RefusingConnector), configured, IDLE)
    }

    /// Opens a callback forward on a free port and returns the port. Retries with another port
    /// when a concurrent test took the port in the meantime, for example as the local port of
    /// a connection.
    async fn ensure_free_port(forwards: &mut CallbackForwards) -> u16 {
        let mut port = free_port().await;

        for _ in 0..10 {
            port = match forwards.ensure(port).await {
                Ok(()) => return port,
                Err(_) => free_port().await,
            };
        }

        panic!("couldn't open a callback forward on a free port");
    }

    /// Opens the forward of the port again `times` times, half an idle timeout apart.
    async fn ensure_repeatedly(forwards: &mut CallbackForwards, port: u16, times: usize) {
        for _ in 0..times {
            tokio::time::sleep(IDLE / 2).await;
            forwards.ensure(port).await.unwrap();
        }
    }

    /// Waits until the connector opened a connection.
    async fn wait_for_connection(connector: &HoldingConnector) {
        while connector.guest_ends.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    }

    /// Whether the host port can be listened on, which means its forward is closed. It doesn't
    /// connect, because a connection would reset the idle timer.
    async fn is_free(port: u16) -> bool {
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await.is_ok()
    }

    /// Waits until the host port can be listened on again, or fails after a few timeouts.
    async fn wait_until_closed(port: u16) {
        let deadline = Instant::now() + 10 * IDLE;

        while !is_free(port).await {
            assert!(Instant::now() < deadline, "localhost:{port} is still open");
            tokio::time::sleep(IDLE / 10).await;
        }
    }

    /// Checks the host port until the returned task is aborted, and returns from the task
    /// whether the port was ever free, which means its forward closed in between.
    fn watch_for_gaps(port: u16) -> tokio::task::JoinHandle<bool> {
        tokio::spawn(wait_until_free(port))
    }

    /// Checks the host port until it's free, and returns `true` then.
    async fn wait_until_free(port: u16) -> bool {
        while !is_free(port).await {
            tokio::time::sleep(IDLE / 20).await;
        }

        true
    }

    #[tokio::test]
    async fn callback_forward_closes_after_idle_timeout_and_opens_again() {
        let mut forwards = callback_forwards();
        let port = ensure_free_port(&mut forwards).await;

        tokio::time::sleep(IDLE / 2).await;
        assert!(!is_free(port).await, "closed before the idle timeout");
        wait_until_closed(port).await;
        forwards.ensure(port).await.unwrap();

        assert!(connect(port).await.is_ok());
    }

    #[tokio::test]
    async fn callback_forward_is_reused_and_its_idle_timer_reset() {
        let mut forwards = callback_forwards();
        let port = ensure_free_port(&mut forwards).await;
        let gaps = watch_for_gaps(port);

        // Without the resets the forward would close after IDLE and open again on the next call.
        ensure_repeatedly(&mut forwards, port, 4).await;

        gaps.abort();
        assert!(
            gaps.await.is_err(),
            "the forward of localhost:{port} closed"
        );
        assert!(connect(port).await.is_ok());
    }

    #[tokio::test]
    async fn callback_forward_stays_open_while_a_connection_is_open() {
        let connector = Arc::new(HoldingConnector::default());
        let configured = Arc::new(Forwards::new(RefusingConnector));
        let mut forwards = CallbackForwards::new("dev", connector.clone(), configured, IDLE);
        let port = ensure_free_port(&mut forwards).await;
        let _open = connect(port).await.unwrap();
        wait_for_connection(&connector).await;

        tokio::time::sleep(3 * IDLE).await;

        assert!(connect(port).await.is_ok());
    }

    #[tokio::test]
    async fn callback_forward_of_a_busy_port_fails() {
        let mut forwards = callback_forwards();
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = busy.local_addr().unwrap().port();

        let err = forwards.ensure(port).await.unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn callback_forward_uses_the_configured_forward_of_the_port() {
        let configured = Arc::new(Forwards::new(RefusingConnector));
        let [same, other] = free_ports().await;
        configured
            .apply("dev", &[mapping(same, same), mapping(other, 1)])
            .await;
        let mut forwards =
            CallbackForwards::new("dev", Arc::new(RefusingConnector), configured, IDLE);

        assert!(forwards.ensure(same).await.is_ok());
        assert_eq!(
            forwards.ensure(other).await.unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
    }

    #[tokio::test]
    async fn dropping_the_callback_forwards_closes_them() {
        let mut forwards = callback_forwards();
        let port = ensure_free_port(&mut forwards).await;

        drop(forwards);
        tokio::task::yield_now().await;

        assert!(connect(port).await.is_err());
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
