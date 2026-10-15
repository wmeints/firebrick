//! The endpoints behind the vsock routes of sandboxes, and the listeners on them.
//!
//! A vsock route connects a guest vsock port on host CID 2 to a host endpoint, a unix socket or
//! on Windows a named pipe (see [`ipc`]), and is fixed when the sandbox is created (ADR 0038).
//! Each sandbox gets its own endpoint per channel, named after the sandbox, so the endpoint tells
//! fbkd which sandbox a connection comes from without trusting the guest to say so. The sockets
//! live in a directory private to the user and never fall back to a shared temp directory; the
//! pipes only let the user in. [`Listeners`] keeps one listener per sandbox for a channel, such
//! as the network log or the clipboard, and [`Connections`] bounds the connections a sandbox
//! has open on it.

use firebrick_utils::ipc;
use microsandbox::sandbox::{SandboxBuilder, SandboxSpec, VsockSocketType};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinHandle, JoinSet};

/// The most connections fbkd accepts from one sandbox on one channel at a time.
pub const MAX_CONNECTIONS: usize = 4;

/// Returns the file `{sandbox}.{extension}` in the directory, or `None` when the sandbox name
/// isn't a single path component and the file would end up elsewhere. fbkd checks names when a
/// request comes in; this keeps a name that slipped through inside the directory.
pub fn sandbox_file(dir: &Path, sandbox: &str, extension: &str) -> Option<PathBuf> {
    if Path::new(sandbox).file_name() != Some(OsStr::new(sandbox)) {
        tracing::error!("refusing to name a file after the invalid sandbox name {sandbox:?}");
        return None;
    }

    Some(dir.join(format!("{sandbox}.{extension}")))
}

/// Returns the endpoint of the sandbox in the directory, or `None` when there's no directory,
/// the name isn't a single path component, or the endpoint name is too long for a unix socket or
/// a pipe, since microsandbox would then refuse to boot the sandbox.
fn socket_path(dir: Option<&Path>, sandbox: &str) -> Option<PathBuf> {
    let dir = dir?;
    sandbox_file(dir, sandbox, "sock")?;

    ipc::endpoint(dir, sandbox)
}

/// Whether the spec has a stream route from the guest port to the socket.
fn routes_to(spec: &SandboxSpec, port: u32, socket: &Path) -> bool {
    spec.vsock.routes.iter().any(|route| {
        route.port == port
            && route.socket_type == VsockSocketType::Stream
            && route.host_socket == socket
    })
}

/// Why there's no endpoint directory.
#[cfg(unix)]
const NO_ENDPOINT_DIR: &str = "XDG_RUNTIME_DIR isn't an absolute path";

/// Why there's no endpoint directory.
#[cfg(windows)]
const NO_ENDPOINT_DIR: &str = "the current user's SID can't be read to name the pipes";

/// Returns the socket directory when it's private to the user, creating it when it's missing.
/// Logs a warning that sandboxes get no `channel` when there's no directory or it isn't private.
fn private_dir(dir: Option<PathBuf>, channel: &str) -> Option<PathBuf> {
    let Some(dir) = dir else {
        tracing::warn!("{NO_ENDPOINT_DIR}, so sandboxes get no {channel}");
        return None;
    };

    match ipc::prepare_dir(&dir) {
        Ok(()) => {
            tracing::info!("{channel} sockets are in {}", dir.display());
            Some(dir)
        }
        Err(err) => {
            tracing::warn!(
                "sandboxes get no {channel}: {err}, or set XDG_RUNTIME_DIR to a private directory"
            );
            None
        }
    }
}

/// Binds the endpoint at `path` in the private directory `dir`. Checks the directory again
/// first, since it may have been removed since fbkd started, for example by the cleanup of
/// `/tmp`. Removes a stale socket file, but fails with [`io::ErrorKind::AddrInUse`] when another
/// process listens on it.
async fn bind(dir: &Path, path: &Path) -> io::Result<ipc::Listener> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || ipc::prepare_dir(&dir))
        .await
        .map_err(io::Error::other)??;

    if ipc::is_served(path).await {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another process listens on {}", path.display()),
        ));
    }

    ipc::remove(path)?;

    ipc::Listener::bind(path)
}

/// Removes the file, if there's one, logging a warning when that fails.
pub async fn remove_logged(path: Option<PathBuf>) {
    let Some(path) = path else {
        return;
    };

    if let Err(err) = remove_if_exists(&path).await {
        tracing::warn!("failed to remove {}: {err}", path.display());
    }
}

/// Removes the file, succeeding when it doesn't exist.
async fn remove_if_exists(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Returns the error of a task that panicked, and nothing for one that ended or was cancelled.
pub fn panicked(result: Result<(), JoinError>) -> Option<JoinError> {
    result.err().filter(JoinError::is_panic)
}

/// Why a connection wasn't accepted.
#[derive(Debug)]
pub enum Refused {
    /// The sandbox already has [`MAX_CONNECTIONS`] connections; the new one was closed.
    Full,
    /// Accepting failed on the host.
    Failed(io::Error),
}

/// The open connections of one sandbox on one channel.
#[derive(Default)]
pub struct Connections {
    tasks: JoinSet<()>,
}

impl Connections {
    /// Runs `serve` on an accepted connection, or closes it when the sandbox already has
    /// [`MAX_CONNECTIONS`] connections.
    pub fn accept<F>(
        &mut self,
        accepted: io::Result<ipc::Stream>,
        serve: impl FnOnce(ipc::Stream) -> F,
    ) -> Result<(), Refused>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match accepted {
            Ok(_) if self.tasks.len() >= MAX_CONNECTIONS => Err(Refused::Full),
            Ok(stream) => {
                self.tasks.spawn(serve(stream));
                Ok(())
            }
            Err(err) => Err(Refused::Failed(err)),
        }
    }

    /// Waits for the next connection to end and returns its result, or `None` when there are no
    /// connections.
    pub async fn join_next(&mut self) -> Option<Result<(), JoinError>> {
        self.tasks.join_next().await
    }
}

/// A sandbox fbkd listens for, and the state its listener shares with the registry.
struct Listening<S> {
    state: Arc<S>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

/// One listener per sandbox on one channel, on a socket per sandbox in a private directory.
pub struct Listeners<S> {
    channel: &'static str,
    port: u32,
    socket_dir: Option<PathBuf>,
    listening: Mutex<HashMap<String, Listening<S>>>,
}

impl<S: Send + Sync + 'static> Listeners<S> {
    /// Creates a registry for the channel, named like `network log` in log lines, whose route
    /// goes from guest vsock port `port` to a socket in `socket_dir`. Without a socket
    /// directory, sandboxes get no route.
    pub fn new(channel: &'static str, port: u32, socket_dir: Option<PathBuf>) -> Self {
        Self {
            channel,
            port,
            socket_dir,
            listening: Mutex::default(),
        }
    }

    /// Creates a registry like [`Listeners::new`] with the sockets in `socket_dir` when it's
    /// private to the user, creating it when it's missing. Leaves out a directory that is
    /// missing or isn't private, with a warning.
    pub fn private(channel: &'static str, port: u32, socket_dir: Option<PathBuf>) -> Self {
        Self::new(channel, port, private_dir(socket_dir, channel))
    }

    /// Locks the registry. It stays consistent when a thread panics while holding it, so a
    /// poisoned lock is safe to use.
    fn listening(&self) -> MutexGuard<'_, HashMap<String, Listening<S>>> {
        self.listening
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns the socket of the sandbox, or `None` when there's no socket directory, the name
    /// isn't a single path component, or the path is too long for a unix socket.
    pub fn socket_path(&self, sandbox: &str) -> Option<PathBuf> {
        socket_path(self.socket_dir.as_deref(), sandbox)
    }

    /// Adds the stream route from the guest port to the sandbox's socket to the builder. Leaves
    /// the builder alone, with a warning, when the sandbox has no socket path, since
    /// microsandbox would then refuse to boot the sandbox.
    pub fn add_route(&self, builder: SandboxBuilder, sandbox: &str) -> SandboxBuilder {
        match self.socket_path(sandbox) {
            Some(path) => builder.vsock(path, self.port),
            None => {
                tracing::warn!(
                    "sandbox {sandbox} has no {} socket path; it gets no route",
                    self.channel
                );
                builder
            }
        }
    }

    /// Whether the sandbox has a stream route from the guest port to its socket.
    pub fn has_route(&self, sandbox: &str, spec: &SandboxSpec) -> bool {
        self.socket_path(sandbox)
            .is_some_and(|path| routes_to(spec, self.port, &path))
    }

    /// Listens on the sandbox's socket unless fbkd already does. A listener that stopped, for
    /// example because it panicked, is replaced. Binds the socket, removing a stale socket file
    /// first but failing when another process listens on it, then creates the listener's state
    /// with `state` and runs `serve` with the listener, the state and a receiver that fires when
    /// the listener should stop.
    pub async fn listen<Fut>(
        &self,
        sandbox: &str,
        state: impl Future<Output = S>,
        serve: impl FnOnce(ipc::Listener, Arc<S>, oneshot::Receiver<()>) -> Fut,
    ) -> io::Result<()>
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        if self.is_listening(sandbox) {
            return Ok(());
        }

        let listener = self.bind(sandbox).await?;
        let state = Arc::new(state.await);
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(serve(listener, Arc::clone(&state), stopped));
        let listening = Listening {
            state,
            shutdown,
            task,
        };

        // Another task may have started listening while this one bound the socket. Its listener
        // lost the path to this one, so this one takes its place.
        let replaced = self.listening().insert(sandbox.to_string(), listening);

        if let Some(replaced) = replaced {
            replaced.task.abort();
        }

        Ok(())
    }

    /// Whether a listener of the sandbox runs. Logs and forgets one that stopped.
    fn is_listening(&self, sandbox: &str) -> bool {
        let mut listening = self.listening();
        let Some(entry) = listening.get(sandbox) else {
            return false;
        };

        if !entry.task.is_finished() {
            return true;
        }

        tracing::warn!(
            "the {} listener of sandbox {sandbox} stopped; listening again",
            self.channel
        );
        listening.remove(sandbox);

        false
    }

    /// Binds the sandbox's socket, removing a stale socket file first.
    async fn bind(&self, sandbox: &str) -> io::Result<ipc::Listener> {
        let (Some(dir), Some(path)) = (&self.socket_dir, self.socket_path(sandbox)) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "there's no private {} socket directory, or the socket path of sandbox \
                     {sandbox} is too long",
                    self.channel
                ),
            ));
        };

        bind(dir, &path).await
    }

    /// Returns the state of the sandbox's listener while fbkd listens for it.
    pub fn state(&self, sandbox: &str) -> Option<Arc<S>> {
        self.listening()
            .get(sandbox)
            .map(|listening| Arc::clone(&listening.state))
    }

    /// Stops listening for the sandbox and waits until its listener has stopped, after running
    /// `before_stop` on its state, then deletes its socket.
    pub async fn remove(&self, sandbox: &str, before_stop: impl FnOnce(&S)) {
        let removed = self.listening().remove(sandbox);

        if let Some(listening) = removed {
            before_stop(&listening.state);
            self.stop(sandbox, listening).await;
        }

        if let Some(path) = self.socket_path(sandbox)
            && let Err(err) = ipc::remove(&path)
        {
            tracing::warn!("failed to remove {}: {err}", path.display());
        }
    }

    /// Stops listening for every sandbox. Run when the daemon shuts down.
    pub async fn close(&self) {
        let listening: Vec<(String, Listening<S>)> = self.listening().drain().collect();

        for (sandbox, listening) in listening {
            self.stop(&sandbox, listening).await;
        }
    }

    /// Stops the listener and waits until it has stopped. Logs a listener that panicked.
    async fn stop(&self, sandbox: &str, listening: Listening<S>) {
        let _ = listening.shutdown.send(());

        if let Some(err) = panicked(listening.task.await) {
            tracing::error!(
                "the {} listener of sandbox {sandbox} failed: {err}",
                self.channel
            );
        }
    }
}

impl<S> Drop for Listeners<S> {
    fn drop(&mut self) {
        let listening = self
            .listening
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);

        for listening in listening.values() {
            listening.task.abort();
        }
    }
}

/// Returns the directory of the `channel` endpoints for tests that keep their files in `dir`, a
/// temp directory of their own: `<dir>/<channel>`, or on Windows a pipe prefix named after `dir`,
/// which is just as unique.
#[cfg(test)]
pub(crate) fn test_endpoint_dir(dir: &Path, channel: &str) -> PathBuf {
    if cfg!(windows) {
        let unique = dir.file_name().unwrap_or_default().to_string_lossy();
        return PathBuf::from(format!(r"\\.\pipe\fbk-test-{unique}-{channel}"));
    }
    dir.join(channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns an endpoint directory unique to `test`: a directory in a new temp directory, or a
    /// pipe prefix on Windows.
    fn endpoint_dir(test: &str) -> (Option<tempfile::TempDir>, PathBuf) {
        if cfg!(windows) {
            let pipe = format!(r"\\.\pipe\fbk-vsock-test-{}-{test}", std::process::id());
            return (None, PathBuf::from(pipe));
        }
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("sockets");
        (Some(temp), dir)
    }

    /// A registry with its endpoints in `dir`.
    fn listeners(dir: &Path) -> Listeners<()> {
        Listeners::new("test", 52999, Some(dir.to_path_buf()))
    }

    /// Accepts connections and closes each one, until accepting fails.
    async fn close_each(mut listener: ipc::Listener) {
        while listener.accept().await.is_ok() {}
    }

    /// Listens for the sandbox with a listener that closes each connection it accepts.
    async fn listen(listeners: &Listeners<()>, sandbox: &str) -> io::Result<()> {
        listeners
            .listen(sandbox, async {}, |listener, _, stopped| async move {
                tokio::select! {
                    () = close_each(listener) => {}
                    _ = stopped => {}
                }
            })
            .await
    }

    #[test]
    fn socket_path_is_none_when_too_long_or_without_a_directory() {
        let (_temp, dir) = endpoint_dir("paths");

        assert_eq!(
            socket_path(Some(&dir), "short"),
            ipc::endpoint(&dir, "short")
        );
        assert!(socket_path(Some(&dir), "short").is_some());
        assert!(socket_path(Some(&dir), &"a".repeat(300)).is_none());
        assert!(socket_path(None, "short").is_none());
    }

    #[test]
    fn socket_path_is_none_for_a_name_that_leaves_the_directory() {
        let (_temp, dir) = endpoint_dir("names");

        for name in ["../../x", "a/b", "..", ".", "", "/etc/x"] {
            assert!(socket_path(Some(&dir), name).is_none(), "{name:?}");
        }
    }

    #[tokio::test]
    async fn listen_leaves_a_live_socket_alone_and_replaces_a_stale_one() {
        let (_temp, dir) = endpoint_dir("replace");
        let (first, second) = (listeners(&dir), listeners(&dir));
        let path = first.socket_path("sb").unwrap();

        listen(&first, "sb").await.unwrap();
        let busy = listen(&second, "sb").await.unwrap_err();
        drop(first);
        // The aborted listener closes its endpoint once the runtime drops its task.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let replaced = listen(&second, "sb").await;

        assert_eq!(busy.kind(), io::ErrorKind::AddrInUse);
        assert!(replaced.is_ok(), "{replaced:?}");
        assert!(ipc::connect(&path).await.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn listen_refuses_a_socket_directory_with_group_or_other_access() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, dir) = endpoint_dir("open");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let listeners = listeners(&dir);

        let error = listen(&listeners, "sb").await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!dir.join("sb.sock").exists());
    }

    #[tokio::test]
    async fn listen_without_a_socket_directory_fails() {
        let listeners: Listeners<()> = Listeners::new("test", 52999, None);

        let error = listen(&listeners, "sb").await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn remove_runs_before_stop_and_deletes_the_socket() {
        let (_temp, dir) = endpoint_dir("remove");
        let listeners = listeners(&dir);
        listen(&listeners, "sb").await.unwrap();
        let mut ran = false;

        listeners.remove("sb", |_| ran = true).await;

        assert!(ran);
        assert!(listeners.state("sb").is_none());
        assert!(!ipc::exists(&listeners.socket_path("sb").unwrap()).await);
    }

    #[cfg(unix)]
    #[test]
    fn private_keeps_only_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let private = temp.path().join("private");
        let open = temp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(private_dir(Some(private.clone()), "test"), Some(private));
        assert_eq!(private_dir(Some(open), "test"), None);
        assert_eq!(private_dir(None, "test"), None);
    }

    #[tokio::test]
    async fn add_route_adds_a_stream_route_to_the_socket() {
        let (_temp, dir) = endpoint_dir("route");
        let listeners = listeners(&dir);
        let builder = listeners.add_route(
            microsandbox::Sandbox::builder("sb").image("ubuntu:26.04"),
            "sb",
        );
        let spec = builder.build().await.unwrap().spec;

        assert!(listeners.has_route("sb", &spec));
        assert!(!listeners.has_route("other", &spec));
    }

    /// Waits until the sender says the connections are released.
    async fn wait_for_release(mut held: tokio::sync::watch::Receiver<bool>) {
        let _ = held.wait_for(|released| *released).await;
    }

    /// Connects `count` clients to the listener at `path` and accepts each into `connections`
    /// with a task that waits for `held` to be released. Returns what each accept returned.
    async fn accept_clients(
        (path, listener): (&Path, &mut ipc::Listener),
        connections: &mut Connections,
        held: &tokio::sync::watch::Receiver<bool>,
        count: usize,
    ) -> Vec<Result<(), Refused>> {
        let mut results = Vec::new();

        for _ in 0..count {
            let _client = ipc::connect(path).await.unwrap();
            let serve = |_| wait_for_release(held.clone());
            results.push(connections.accept(listener.accept().await, serve));
        }

        results
    }

    #[tokio::test]
    async fn connections_close_the_ones_beyond_the_limit() {
        let (_temp, dir) = endpoint_dir("limit");
        ipc::prepare_dir(&dir).unwrap();
        let path = ipc::endpoint(&dir, "sb").unwrap();
        let mut listener = ipc::Listener::bind(&path).unwrap();
        let mut connections = Connections::default();
        let (release, held) = tokio::sync::watch::channel(false);

        let socket = (path.as_path(), &mut listener);
        let results = accept_clients(socket, &mut connections, &held, MAX_CONNECTIONS + 1).await;
        release.send(true).unwrap();

        assert!(results[..MAX_CONNECTIONS].iter().all(Result::is_ok));
        assert!(matches!(results[MAX_CONNECTIONS], Err(Refused::Full)));
        assert!(connections.join_next().await.unwrap().is_ok());
    }
}
