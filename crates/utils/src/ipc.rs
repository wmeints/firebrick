//! Local endpoints between `fbk`, `fbkd` and the microsandbox processes: unix sockets on Unix
//! and named pipes on Windows (ADR 0050).
//!
//! An endpoint is a [`Path`] on both platforms: a socket file such as
//! `/run/user/1000/fbkd.sock`, or a pipe name such as `\\.\pipe\firebrick-S-1-5-21-..-fbkd`,
//! which microsandbox also takes as the host end of a vsock route. Both sides check the peer:
//! [`Listener::accept`] refuses clients and [`connect`] refuses servers that don't run as the
//! current user or as root (`LocalSystem` on Windows), so a process of another local user can
//! neither call the daemon nor pose as it.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

/// A byte stream that can be read and written asynchronously.
trait Io: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// A connection to or from a peer that runs as the current user or as root.
pub struct Stream(Box<dyn Io>);

impl Stream {
    /// Wraps a connected socket or pipe.
    fn new(io: impl AsyncRead + AsyncWrite + Send + Unpin + 'static) -> Self {
        Self(Box::new(io))
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

/// Listens on an endpoint for clients that run as the current user or as root.
pub struct Listener(sys::Listener);

impl Listener {
    /// Starts listening on the endpoint so that only the current user and root can connect: a
    /// socket of mode `0600` on Unix, a pipe whose access list holds only the user and
    /// `LocalSystem` on Windows. Fails with [`io::ErrorKind::AddrInUse`] when the endpoint
    /// already exists; on Unix that includes a stale socket file, which [`remove`] deletes.
    pub fn bind(path: &Path) -> io::Result<Self> {
        sys::Listener::bind(path).map(Self)
    }

    /// Waits for the next client. A client that runs as another user is disconnected and
    /// returned as an [`io::ErrorKind::PermissionDenied`] error naming it, after which the
    /// listener keeps accepting. Cancel safe: dropping the future loses no client.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        self.0.accept().await
    }
}

/// Connects to the endpoint. Fails with [`io::ErrorKind::PermissionDenied`] when the process
/// that serves it doesn't run as the current user or as root, so a client never sends secrets
/// to an endpoint that another user created first.
pub async fn connect(path: &Path) -> io::Result<Stream> {
    sys::connect(path).await
}

/// Whether a process listens on the endpoint. Connects briefly to find out, without checking who
/// serves it.
pub async fn is_served(path: &Path) -> bool {
    sys::is_served(path).await
}

/// Whether the endpoint exists: a socket file on Unix, live or stale, and a pipe on Windows,
/// which disappears with its server.
pub async fn exists(path: &Path) -> bool {
    sys::exists(path).await
}

/// Removes a stale endpoint. Succeeds when there's none, and on Windows, where a pipe
/// disappears with its server, it does nothing.
pub fn remove(path: &Path) -> io::Result<()> {
    sys::remove(path)
}

/// Returns the endpoint named `name` among the endpoints in `dir`, or `None` when `name` isn't a
/// single path component or the endpoint name gets too long.
///
/// On Unix `dir` is a directory and the endpoint the socket `<dir>/<name>.sock`, which must fit
/// in a unix socket address. On Windows `dir` is a pipe name that the endpoints extend to
/// `<dir>-<name>`, because a pipe name has no directories.
pub fn endpoint(dir: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || Path::new(name).file_name() != Some(name.as_ref()) {
        return None;
    }

    sys::endpoint(dir, name)
}

/// Prepares the directory that [`endpoint`] puts the endpoints of `dir` in. On Unix it creates
/// the directory private to the user, like [`crate::create_private_dir`], and fails when it isn't
/// private; on Windows, where a pipe protects itself, it does nothing.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    sys::prepare_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Returns an endpoint directory unique to this test and the directory it lives in, if any.
    fn endpoint_dir(test: &str) -> (Option<tempfile::TempDir>, PathBuf) {
        if cfg!(windows) {
            let pipe = format!(r"\\.\pipe\fbk-ipc-test-{}-{test}", std::process::id());
            return (None, PathBuf::from(pipe));
        }
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("endpoints");
        (Some(temp), dir)
    }

    /// Returns the endpoint `name` of a prepared directory unique to this test.
    fn test_endpoint(test: &str) -> (Option<tempfile::TempDir>, PathBuf) {
        let (temp, dir) = endpoint_dir(test);
        prepare_dir(&dir).unwrap();
        (temp, endpoint(&dir, "sb").unwrap())
    }

    /// Accepts one client, reads four bytes from it, answers `pong` and returns what it read.
    async fn answer_pong(mut listener: Listener) -> [u8; 4] {
        let mut stream = listener.accept().await.unwrap();
        let mut buf = [0; 4];
        stream.read_exact(&mut buf).await.unwrap();
        stream.write_all(b"pong").await.unwrap();
        buf
    }

    /// Accepts `count` clients one after another and writes one byte to each.
    async fn greet_clients(mut listener: Listener, count: usize) {
        for _ in 0..count {
            let mut stream = listener.accept().await.unwrap();
            stream.write_all(b"x").await.unwrap();
        }
    }

    /// Connects to the endpoint `count` times one after another and reads one byte each time.
    async fn read_greetings(path: &Path, count: usize) {
        for _ in 0..count {
            let mut client = connect(path).await.unwrap();
            let mut buf = [0; 1];
            client.read_exact(&mut buf).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_client_of_the_same_user_exchanges_bytes_with_the_listener() {
        let (_temp, path) = test_endpoint("exchange");
        let server = tokio::spawn(answer_pong(Listener::bind(&path).unwrap()));

        let mut client = connect(&path).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut reply = [0; 4];
        client.read_exact(&mut reply).await.unwrap();

        assert_eq!(&server.await.unwrap(), b"ping");
        assert_eq!(&reply, b"pong");
    }

    #[tokio::test]
    async fn the_listener_accepts_one_client_after_another() {
        let (_temp, path) = test_endpoint("sequence");
        let server = tokio::spawn(greet_clients(Listener::bind(&path).unwrap(), 3));

        read_greetings(&path, 3).await;

        server.await.unwrap();
    }

    #[tokio::test]
    async fn bind_fails_while_another_listener_serves_the_endpoint() {
        let (_temp, path) = test_endpoint("busy");
        let _listener = Listener::bind(&path).unwrap();

        let err = Listener::bind(&path).err().unwrap();

        assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{err}");
        assert!(is_served(&path).await);
        assert!(exists(&path).await);
    }

    #[tokio::test]
    async fn an_endpoint_nobody_serves_is_not_served_and_refuses_clients() {
        let (_temp, path) = test_endpoint("missing");

        assert!(!is_served(&path).await);
        assert!(!exists(&path).await);
        assert!(connect(&path).await.is_err());
        remove(&path).unwrap();
    }

    #[tokio::test]
    async fn a_removed_endpoint_can_be_bound_again() {
        let (_temp, path) = test_endpoint("rebind");
        drop(Listener::bind(&path).unwrap());

        remove(&path).unwrap();

        assert!(!exists(&path).await);
        assert!(Listener::bind(&path).is_ok());
    }

    #[test]
    fn endpoint_is_none_for_a_name_that_is_not_one_component() {
        let (_temp, dir) = endpoint_dir("names");

        for name in ["../../x", "a/b", "..", ".", "", "/etc/x"] {
            assert_eq!(endpoint(&dir, name), None, "{name:?}");
        }
    }

    #[test]
    fn endpoint_is_none_when_the_name_gets_too_long() {
        let (_temp, dir) = endpoint_dir("long");

        assert!(endpoint(&dir, "short").is_some());
        assert_eq!(endpoint(&dir, &"a".repeat(300)), None);
    }
}
