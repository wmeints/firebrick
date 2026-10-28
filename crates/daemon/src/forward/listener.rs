//! The loopback listeners behind [`super::Forwards`] and [`super::CallbackForwards`]: binds a
//! host port, accepts its connections and relays each one to the guest port through a
//! [`Connector`], and tracks when a forward is idle.

use firebrick_spec::PortMapping;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::JoinSet;

use super::Connector;

/// The listeners of one forward. Dropping it closes them and their connections.
pub(super) struct Forward(JoinSet<()>);

/// Where a forward sends its connections, and what tracks whether it's idle.
#[derive(Clone)]
pub(super) struct Route {
    pub(super) sandbox: String,
    pub(super) port: PortMapping,
    pub(super) connector: Arc<dyn Connector>,
    pub(super) activity: Arc<Activity>,
}

/// Listens on the route's host port and forwards its connections to the guest port, until the
/// forward is dropped or the route's activity says it's idle.
pub(super) async fn listen(route: Route) -> io::Result<Forward> {
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

/// When a forward was last used and how many connections it has open, to close it after it's
/// idle.
pub(super) struct Activity {
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
    pub(super) fn new(idle_timeout: Option<Duration>) -> Self {
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
    pub(super) fn touch(&self) -> bool {
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

/// Stops the listeners of a forward and waits until they are closed.
pub(super) async fn close(mut forward: Forward) {
    forward.0.shutdown().await;
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

/// Binds a socket to the address with `SO_REUSEADDR`, without listening, and closes it. On
/// Windows `SO_REUSEADDR` lets a bind succeed even next to an active listener, so the probe
/// binds without it there, which fails next to a listener just the same.
fn probe_bind(address: SocketAddr) -> io::Result<()> {
    let socket = match address {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    #[cfg(unix)]
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
