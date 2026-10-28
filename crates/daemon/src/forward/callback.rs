//! Forwards of one sandbox that open on demand, such as for the OAuth callback port of a URL
//! the sandbox opens on the host, and close after they're idle.

use firebrick_spec::PortMapping;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use super::listener::{Activity, Forward, Route, close, listen};
use super::{Connector, Forwards, open_failed};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forward::GuestIo;
    use crate::forward::tests::{connect, free_port, free_ports, mapping};
    use async_trait::async_trait;
    use std::io;
    use std::net::Ipv4Addr;
    use std::time::Instant;
    use tokio::io::DuplexStream;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

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
}
