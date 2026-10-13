//! Shares the host clipboard with the sandboxes created with `clipboard: true`.
//!
//! The `wl-paste` and `xclip` stand-ins in the `firebrick-base` image connect to vsock port
//! [`GUEST_PORT`] on the host. Sandboxes with the [`ENABLED_LABEL`] get a stream route from that
//! port to their own unix socket in [`firebrick_utils::clipboard_socket_dir`], where fbkd
//! listens; other sandboxes get no route, so nothing in their guest reaches the clipboard, not
//! even root. The label also gives the guest the [`MARKER`] script, so the stand-ins can tell a
//! sandbox without the setting from one whose fbkd doesn't answer.
//!
//! Each connection carries one request line, `types`, `text` or `png`, and gets one answer:
//! `ok <length>` followed by that many bytes, or one of `empty`, `too-large`, `unavailable` and
//! `invalid`. fbkd reads the host clipboard through a [`HostClipboard`] only when asked, and
//! never sends more than [`MAX_TEXT`] bytes of text or [`MAX_PNG`] bytes of PNG.

use crate::vsock;
use async_trait::async_trait;
use microsandbox::sandbox::{SandboxBuilder, SandboxSpec};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

/// Guest vsock port the stand-ins connect to, on host CID 2.
pub const GUEST_PORT: u32 = 52001;

/// Sandbox label that records whether the sandbox shares the host clipboard. Only sandboxes
/// created with the setting have it, set to `true`.
pub const ENABLED_LABEL: &str = "firebrick.clipboard";

/// Name of the microsandbox script that marks a sandbox with the setting. microsandbox puts it
/// in the guest at `/.msb/scripts/<name>`.
pub const MARKER: &str = "firebrick-clipboard";

/// Content of the [`MARKER`] script.
const MARKER_SCRIPT: &str = "#!/bin/sh\n\
    # fbkd shares the host clipboard with this sandbox on vsock port 52001.\n\
    echo 'clipboard sharing is on for this sandbox'\n";

/// The most bytes of text fbkd sends.
pub const MAX_TEXT: usize = 1024 * 1024;

/// The most bytes of PNG fbkd sends.
pub const MAX_PNG: usize = 10 * 1024 * 1024;

/// The most connections fbkd accepts from one sandbox at a time.
const MAX_CONNECTIONS: usize = 4;

/// The longest request line fbkd reads, in bytes, without its newline.
const MAX_REQUEST: usize = 16;

/// How long a stand-in gets to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long reading the host clipboard may take. The stand-ins give up after 5 seconds.
const HOST_TIMEOUT: Duration = Duration::from_secs(4);

/// How often fbkd logs a warning about one sandbox's clipboard at most.
const WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// The most bytes of a host tool's stderr fbkd keeps to log.
const MAX_STDERR: usize = 1024;

/// Returns whether a sandbox with the labels shares the host clipboard.
pub fn is_enabled(labels: &BTreeMap<String, String>) -> bool {
    labels.get(ENABLED_LABEL).map(String::as_str) == Some("true")
}

/// A kind of clipboard content the sandbox can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// UTF-8 text.
    Text,
    /// A PNG image.
    Png,
}

impl Kind {
    /// Returns the most bytes of the kind fbkd sends.
    pub fn limit(self) -> usize {
        match self {
            Kind::Text => MAX_TEXT,
            Kind::Png => MAX_PNG,
        }
    }

    /// Returns the name of the kind in the protocol.
    fn name(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Png => "png",
        }
    }
}

/// Why the host clipboard can't give what was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// The clipboard is empty or doesn't hold the kind.
    #[error("the clipboard holds no such content")]
    Empty,
    /// The content is larger than the kind's limit.
    #[error("the clipboard content is too large")]
    TooLarge,
    /// The clipboard can't be read, for example because there's no display.
    #[error("{0}")]
    Unavailable(String),
}

/// Reads the clipboard of the host.
#[async_trait]
pub trait HostClipboard: Send + Sync {
    /// Returns the kinds the clipboard holds, empty when it's empty.
    async fn kinds(&self) -> Result<Vec<Kind>, HostError>;

    /// Returns the content of the kind, failing with [`HostError::TooLarge`] without reading
    /// much more than `limit` bytes when it's larger.
    async fn read(&self, kind: Kind, limit: usize) -> Result<Vec<u8>, HostError>;
}

/// A request from a stand-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    /// The kinds the clipboard holds.
    Types,
    /// The content of a kind.
    Read(Kind),
}

/// Parses a request line without its newline.
fn parse_request(line: &[u8]) -> Option<Request> {
    match line {
        b"types" => Some(Request::Types),
        b"text" => Some(Request::Read(Kind::Text)),
        b"png" => Some(Request::Read(Kind::Png)),
        _ => None,
    }
}

/// The answer to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reply {
    /// The content, or the names of the kinds one per line.
    Content(Vec<u8>),
    /// The clipboard doesn't hold the kind.
    Empty,
    /// The content is larger than the kind's limit.
    TooLarge,
    /// The host clipboard can't be read.
    Unavailable,
    /// The request isn't one fbkd knows.
    Invalid,
}

impl Reply {
    /// Returns the reply as it's sent: a status line, followed by the content.
    fn encode(self) -> Vec<u8> {
        let status = match &self {
            Reply::Content(content) => format!("ok {}\n", content.len()),
            Reply::Empty => "empty\n".to_string(),
            Reply::TooLarge => "too-large\n".to_string(),
            Reply::Unavailable => "unavailable\n".to_string(),
            Reply::Invalid => "invalid\n".to_string(),
        };
        let mut encoded = status.into_bytes();

        if let Reply::Content(content) = self {
            encoded.extend(content);
        }

        encoded
    }
}

/// The clipboard channel of one sandbox.
struct Channel {
    sandbox: String,
    host: Arc<dyn HostClipboard>,
    /// When fbkd last logged a warning about the sandbox's clipboard.
    last_warning: Mutex<Option<Instant>>,
}

impl Channel {
    /// Answers a request from the sandbox.
    async fn answer(&self, request: Request) -> Reply {
        let result = match request {
            Request::Types => self.host.kinds().await.map(|kinds| kind_list(&kinds)),
            Request::Read(kind) => self.read(kind).await,
        };

        match result {
            Ok(content) => Reply::Content(content),
            Err(HostError::Empty) => Reply::Empty,
            Err(HostError::TooLarge) => Reply::TooLarge,
            Err(HostError::Unavailable(reason)) => {
                self.warn(&format!("the host clipboard isn't available: {reason}"));
                Reply::Unavailable
            }
        }
    }

    /// Reads the kind, refusing content over its limit also when the host clipboard returns it.
    async fn read(&self, kind: Kind) -> Result<Vec<u8>, HostError> {
        let limit = kind.limit();
        let content = self.host.read(kind, limit).await?;

        if content.len() > limit {
            return Err(HostError::TooLarge);
        }

        Ok(content)
    }

    /// Logs a warning about the sandbox, at most once per [`WARNING_INTERVAL`].
    fn warn(&self, message: &str) {
        let mut last = self
            .last_warning
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();

        if last.is_some_and(|last| now.duration_since(last) < WARNING_INTERVAL) {
            return;
        }

        *last = Some(now);
        tracing::warn!("clipboard of sandbox {}: {message}", self.sandbox);
    }
}

/// Returns the names of the kinds, one per line.
fn kind_list(kinds: &[Kind]) -> Vec<u8> {
    kinds
        .iter()
        .map(|kind| format!("{}\n", kind.name()))
        .collect::<String>()
        .into_bytes()
}

/// Reads the request of one connection and writes the answer.
async fn serve_connection(stream: UnixStream, channel: Arc<Channel>) {
    let (read, mut write) = stream.into_split();
    let request = tokio::time::timeout(REQUEST_TIMEOUT, read_request(read)).await;
    let reply = match request {
        Ok(Ok(Some(request))) => channel.answer(request).await,
        Ok(Ok(None)) => Reply::Invalid,
        // The stand-in went away or took too long; there's nobody to answer.
        Ok(Err(_)) | Err(_) => return,
    };

    if let Err(err) = write.write_all(&reply.encode()).await {
        tracing::debug!(
            "failed to answer a clipboard request of sandbox {}: {err}",
            channel.sandbox
        );
    }
}

/// Reads the request line, at most [`MAX_REQUEST`] bytes of it. Returns `None` when it isn't a
/// known request.
async fn read_request(read: impl AsyncRead + Unpin) -> io::Result<Option<Request>> {
    let limit = u64::try_from(MAX_REQUEST + 1).unwrap_or(u64::MAX);
    let mut line = Vec::with_capacity(MAX_REQUEST + 1);
    BufReader::new(read.take(limit))
        .read_until(b'\n', &mut line)
        .await?;

    Ok(line.strip_suffix(b"\n").and_then(parse_request))
}

/// Accepts connections from the sandbox until `shutdown` fires or its sender is dropped, and
/// answers them. Closes connections beyond [`MAX_CONNECTIONS`].
async fn serve(listener: UnixListener, channel: Arc<Channel>, shutdown: oneshot::Receiver<()>) {
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(_) if connections.len() >= MAX_CONNECTIONS => channel.warn(&format!(
                    "closed a connection beyond the limit of {MAX_CONNECTIONS}"
                )),
                Ok((stream, _)) => {
                    connections.spawn(serve_connection(stream, Arc::clone(&channel)));
                }
                Err(err) => channel.warn(&format!("failed to accept a connection: {err}")),
            },
            Some(result) = connections.join_next() => log_panic(&channel.sandbox, result),
            _ = &mut shutdown => return,
        }
    }
}

/// Logs a task that panicked.
fn log_panic(sandbox: &str, result: Result<(), tokio::task::JoinError>) {
    if let Err(err) = result
        && err.is_panic()
    {
        tracing::error!("the clipboard listener of sandbox {sandbox} failed: {err}");
    }
}

/// A sandbox fbkd listens for.
struct Listening {
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Listening {
    /// Stops listening and waits until the listener has stopped.
    async fn stop(self, sandbox: &str) {
        let _ = self.shutdown.send(());
        log_panic(sandbox, self.task.await);
    }
}

/// Listens for the clipboard requests of the sandboxes that share the host clipboard.
pub struct Clipboards {
    socket_dir: Option<PathBuf>,
    host: Arc<dyn HostClipboard>,
    listening: Mutex<HashMap<String, Listening>>,
}

impl Clipboards {
    /// Creates a registry with the sockets in `socket_dir` that reads the clipboard from
    /// `host`. Without a socket directory, sandboxes get no route.
    pub fn new(socket_dir: Option<PathBuf>, host: Arc<dyn HostClipboard>) -> Self {
        Self {
            socket_dir,
            host,
            listening: Mutex::default(),
        }
    }

    /// Creates a registry with the sockets in the directory the environment selects, see
    /// [`firebrick_utils::clipboard_socket_dir`]. Leaves out a directory that isn't private to
    /// the user, with a warning.
    pub fn from_env(host: Arc<dyn HostClipboard>) -> Self {
        let dir = vsock::private_dir(firebrick_utils::clipboard_socket_dir(), "clipboard");

        Self::new(dir, host)
    }

    /// Locks the registry. It stays consistent when a thread panics while holding it, so a
    /// poisoned lock is safe to use.
    fn listening(&self) -> MutexGuard<'_, HashMap<String, Listening>> {
        self.listening
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns the socket of the sandbox, or `None` when there's no socket directory or the
    /// path is too long for a unix socket.
    pub fn socket_path(&self, sandbox: &str) -> Option<PathBuf> {
        vsock::socket_path(self.socket_dir.as_deref(), sandbox)
    }

    /// Adds the [`MARKER`] script and the route from [`GUEST_PORT`] to the sandbox's socket to
    /// the builder. Leaves out the route, with a warning, when the sandbox has no socket path,
    /// since microsandbox would then refuse to boot the sandbox.
    pub fn add_route(&self, builder: SandboxBuilder, sandbox: &str) -> SandboxBuilder {
        let builder = builder.script(MARKER, MARKER_SCRIPT);

        match self.socket_path(sandbox) {
            Some(path) => builder.vsock(path, GUEST_PORT),
            None => {
                tracing::warn!("sandbox {sandbox} has no clipboard socket path; it gets no route");
                builder
            }
        }
    }

    /// Whether the sandbox has a stream route from [`GUEST_PORT`] to its socket.
    pub fn has_route(&self, sandbox: &str, spec: &SandboxSpec) -> bool {
        self.socket_path(sandbox)
            .is_some_and(|path| vsock::has_route(spec, GUEST_PORT, &path))
    }

    /// Listens on the sandbox's socket unless fbkd already does. A listener that stopped is
    /// replaced. Logs an error when the socket can't be bound: the clipboard never stops a
    /// sandbox from starting.
    pub async fn listen(&self, sandbox: &str) {
        if self.is_listening(sandbox) {
            return;
        }

        let listener = match self.bind(sandbox).await {
            Ok(listener) => listener,
            Err(err) => {
                tracing::error!("failed to listen for the clipboard of sandbox {sandbox}: {err}");
                return;
            }
        };
        let channel = Arc::new(Channel {
            sandbox: sandbox.to_string(),
            host: Arc::clone(&self.host),
            last_warning: Mutex::new(None),
        });
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(serve(listener, channel, stopped));

        // Another task may have started listening while this one bound the socket. Its listener
        // lost the path to this one, so this one takes its place.
        let replaced = self
            .listening()
            .insert(sandbox.to_string(), Listening { shutdown, task });

        if let Some(replaced) = replaced {
            replaced.task.abort();
        }
    }

    /// Whether a listener of the sandbox runs. Forgets one that stopped.
    fn is_listening(&self, sandbox: &str) -> bool {
        let mut listening = self.listening();

        match listening.get(sandbox) {
            Some(entry) if !entry.task.is_finished() => true,
            Some(_) => {
                listening.remove(sandbox);
                false
            }
            None => false,
        }
    }

    /// Binds the sandbox's socket, removing a stale socket file first.
    async fn bind(&self, sandbox: &str) -> io::Result<UnixListener> {
        let (Some(dir), Some(path)) = (&self.socket_dir, self.socket_path(sandbox)) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "there's no socket directory or the socket path is too long",
            ));
        };

        vsock::bind(dir, &path).await
    }

    /// Stops listening for the sandbox and deletes its socket.
    pub async fn remove(&self, sandbox: &str) {
        let removed = self.listening().remove(sandbox);

        if let Some(listening) = removed {
            listening.stop(sandbox).await;
        }

        vsock::remove_logged(self.socket_path(sandbox)).await;
    }

    /// Stops listening for every sandbox. Run when the daemon shuts down.
    pub async fn close(&self) {
        let listening: Vec<(String, Listening)> = self.listening().drain().collect();

        for (sandbox, listening) in listening {
            listening.stop(&sandbox).await;
        }
    }
}

impl Drop for Clipboards {
    fn drop(&mut self) {
        for listening in self.listening().values() {
            listening.task.abort();
        }
    }
}

/// Reads the host clipboard with the tools of the desktop: `wl-paste` on Wayland, `xclip` on
/// X11, and `pbpaste` and `osascript` on macOS. fbkd inherits the display of the `fbk` that
/// started it, so it reads the clipboard of that desktop session.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostTools;

#[async_trait]
impl HostClipboard for HostTools {
    async fn kinds(&self) -> Result<Vec<Kind>, HostError> {
        let backend = Backend::from_env()?;
        let (program, args) = backend.list_command();
        let output = run(program, args, MAX_TYPES).await?;

        if !output.success {
            return backend.empty_or_unavailable(program, &output.stderr);
        }

        let text = String::from_utf8_lossy(&output.stdout);

        Ok(match backend {
            Backend::MacOs => kinds_from_clipboard_info(&text),
            Backend::Wayland | Backend::X11 => kinds_from_types(&text),
        })
    }

    async fn read(&self, kind: Kind, limit: usize) -> Result<Vec<u8>, HostError> {
        if !self.kinds().await?.contains(&kind) {
            return Err(HostError::Empty);
        }

        let backend = Backend::from_env()?;
        let (program, args) = backend.read_command(kind);
        let output = run(program, args, backend.output_limit(kind, limit)).await?;

        if !output.success {
            return backend.empty_or_unavailable(program, &output.stderr);
        }

        backend.content(kind, output.stdout, limit)
    }
}

/// The most bytes of the list of types fbkd reads.
const MAX_TYPES: usize = 64 * 1024;

/// The clipboard tools of a desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Wayland,
    X11,
    MacOs,
}

impl Backend {
    /// Returns the backend of the environment fbkd runs in.
    fn from_env() -> Result<Self, HostError> {
        Self::detect(cfg!(target_os = "macos"), |name| {
            std::env::var_os(name).is_some_and(|value| !value.is_empty())
        })
    }

    /// Returns the backend for macOS when `macos` is set, and otherwise by the display
    /// variables that `is_set` reports.
    fn detect(macos: bool, is_set: impl Fn(&str) -> bool) -> Result<Self, HostError> {
        if macos {
            Ok(Backend::MacOs)
        } else if is_set("WAYLAND_DISPLAY") {
            Ok(Backend::Wayland)
        } else if is_set("DISPLAY") {
            Ok(Backend::X11)
        } else {
            Err(HostError::Unavailable(
                "neither WAYLAND_DISPLAY nor DISPLAY is set; start fbkd from your desktop \
                 session by running fbk there first"
                    .to_string(),
            ))
        }
    }

    /// Returns the command that lists the types on the clipboard.
    fn list_command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Backend::Wayland => ("wl-paste", &["--list-types"]),
            Backend::X11 => ("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]),
            Backend::MacOs => ("osascript", &["-e", "clipboard info"]),
        }
    }

    /// Returns the command that prints the content of the kind.
    fn read_command(self, kind: Kind) -> (&'static str, &'static [&'static str]) {
        match (self, kind) {
            (Backend::Wayland, Kind::Text) => ("wl-paste", &["--no-newline", "--type", "text"]),
            (Backend::Wayland, Kind::Png) => ("wl-paste", &["--type", "image/png"]),
            (Backend::X11, Kind::Text) => (
                "xclip",
                &["-selection", "clipboard", "-t", "UTF8_STRING", "-o"],
            ),
            (Backend::X11, Kind::Png) => (
                "xclip",
                &["-selection", "clipboard", "-t", "image/png", "-o"],
            ),
            (Backend::MacOs, Kind::Text) => ("pbpaste", &[]),
            (Backend::MacOs, Kind::Png) => ("osascript", &["-e", "the clipboard as «class PNGf»"]),
        }
    }

    /// Returns the most bytes of output to read for content of at most `limit` bytes. AppleScript
    /// prints PNG data as hex.
    fn output_limit(self, kind: Kind, limit: usize) -> usize {
        match (self, kind) {
            (Backend::MacOs, Kind::Png) => limit.saturating_mul(2).saturating_add(64),
            _ => limit,
        }
    }

    /// Returns the content from the command's output.
    fn content(self, kind: Kind, output: Vec<u8>, limit: usize) -> Result<Vec<u8>, HostError> {
        let content = match (self, kind) {
            (Backend::MacOs, Kind::Png) => png_from_applescript(&String::from_utf8_lossy(&output))
                .ok_or_else(|| {
                    HostError::Unavailable("osascript printed no PNG data".to_string())
                })?,
            _ => output,
        };

        if content.len() > limit {
            return Err(HostError::TooLarge);
        }

        Ok(content)
    }

    /// Returns the error of a tool that failed: an empty clipboard when its message says so,
    /// otherwise the clipboard isn't available.
    fn empty_or_unavailable<T>(self, program: &str, stderr: &str) -> Result<T, HostError> {
        let empty = ["Nothing is copied", "No selection", "not available"]
            .iter()
            .any(|message| stderr.contains(message));

        if empty {
            return Err(HostError::Empty);
        }

        Err(HostError::Unavailable(format!(
            "{program} failed: {}",
            firebrick_utils::strip_control(stderr.trim())
        )))
    }
}

/// Returns the kinds of the types that `wl-paste --list-types` or xclip's `TARGETS` lists.
fn kinds_from_types(types: &str) -> Vec<Kind> {
    let types: Vec<&str> = types.lines().map(str::trim).collect();
    let text = types.iter().any(|name| {
        name.to_ascii_lowercase().starts_with("text/plain")
            || ["UTF8_STRING", "STRING", "TEXT"].contains(name)
    });
    let png = types.contains(&"image/png");

    kinds_where(|kind| match kind {
        Kind::Text => text,
        Kind::Png => png,
    })
}

/// Returns the kinds of the classes that AppleScript's `clipboard info` lists.
fn kinds_from_clipboard_info(info: &str) -> Vec<Kind> {
    let text = ["«class utf8»", "Unicode text", "string"]
        .iter()
        .any(|class| info.contains(class));
    let png = info.contains("«class PNGf»");

    kinds_where(|kind| match kind {
        Kind::Text => text,
        Kind::Png => png,
    })
}

/// Returns the kinds that are present, text first.
fn kinds_where(present: impl Fn(Kind) -> bool) -> Vec<Kind> {
    [Kind::Text, Kind::Png]
        .into_iter()
        .filter(|kind| present(*kind))
        .collect()
}

/// Decodes the `«data PNGf<hex>»` that AppleScript prints for PNG data.
fn png_from_applescript(output: &str) -> Option<Vec<u8>> {
    let hex = output
        .trim()
        .strip_prefix("«data PNGf")?
        .strip_suffix('»')?
        .as_bytes();

    if hex.len() % 2 != 0 {
        return None;
    }

    hex.chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

/// The output of a host tool.
struct Output {
    success: bool,
    stdout: Vec<u8>,
    stderr: String,
}

/// Runs the program and returns its output, failing with [`HostError::TooLarge`] as soon as it
/// prints more than `limit` bytes, and with [`HostError::Unavailable`] when it can't run or
/// takes longer than [`HOST_TIMEOUT`]. The program is killed when it's given up on.
async fn run(program: &str, args: &[&str], limit: usize) -> Result<Output, HostError> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| tool_error(program, &format!("can't run: {err}")))?;

    tokio::time::timeout(HOST_TIMEOUT, collect(program, child, limit))
        .await
        .map_err(|_| tool_error(program, &format!("didn't finish within {HOST_TIMEOUT:?}")))?
}

/// Reads the output of the program's child and waits for it to exit, failing with
/// [`HostError::TooLarge`] as soon as it prints more than `limit` bytes.
async fn collect(
    program: &str,
    mut child: tokio::process::Child,
    limit: usize,
) -> Result<Output, HostError> {
    let (stdout, stderr) = tokio::join!(
        read_at_most(child.stdout.take(), limit.saturating_add(1)),
        read_at_most(child.stderr.take(), MAX_STDERR)
    );
    let stdout = stdout.map_err(|err| tool_error(program, &format!("can't be read: {err}")))?;

    if stdout.len() > limit {
        return Err(HostError::TooLarge);
    }

    let status = child
        .wait()
        .await
        .map_err(|err| tool_error(program, &format!("failed: {err}")))?;

    Ok(Output {
        success: status.success(),
        stdout,
        stderr: String::from_utf8_lossy(&stderr.unwrap_or_default()).into_owned(),
    })
}

/// Returns the error for a host tool that couldn't give the clipboard's content.
fn tool_error(program: &str, reason: &str) -> HostError {
    HostError::Unavailable(format!("{program} {reason}"))
}

/// Reads at most `limit` bytes from the pipe, which is empty when there's none.
async fn read_at_most(pipe: Option<impl AsyncRead + Unpin>, limit: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();

    if let Some(pipe) = pipe {
        let limit = u64::try_from(limit).unwrap_or(u64::MAX);
        pipe.take(limit).read_to_end(&mut buf).await?;
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host clipboard with fixed content, or none at all.
    struct FakeClipboard {
        text: Option<Vec<u8>>,
        png: Option<Vec<u8>>,
        available: bool,
        /// Releases reads when they should finish; `None` answers at once.
        gate: Option<tokio::sync::Semaphore>,
    }

    impl FakeClipboard {
        fn with(text: Option<&[u8]>, png: Option<&[u8]>) -> Self {
            Self {
                text: text.map(<[u8]>::to_vec),
                png: png.map(<[u8]>::to_vec),
                available: true,
                gate: None,
            }
        }
    }

    #[async_trait]
    impl HostClipboard for FakeClipboard {
        async fn kinds(&self) -> Result<Vec<Kind>, HostError> {
            if !self.available {
                return Err(HostError::Unavailable("no display".to_string()));
            }

            Ok(kinds_where(|kind| match kind {
                Kind::Text => self.text.is_some(),
                Kind::Png => self.png.is_some(),
            }))
        }

        async fn read(&self, kind: Kind, _limit: usize) -> Result<Vec<u8>, HostError> {
            if let Some(gate) = &self.gate {
                let _permit = gate.acquire().await;
            }

            self.kinds().await?;
            let content = match kind {
                Kind::Text => &self.text,
                Kind::Png => &self.png,
            };

            content.clone().ok_or(HostError::Empty)
        }
    }

    fn clipboards(dir: &std::path::Path, host: FakeClipboard) -> Clipboards {
        Clipboards::new(Some(dir.join("clipboard")), Arc::new(host))
    }

    async fn connect(clipboards: &Clipboards, sandbox: &str) -> io::Result<UnixStream> {
        UnixStream::connect(clipboards.socket_path(sandbox).unwrap()).await
    }

    /// Sends the request, ends the input and returns the whole answer.
    async fn ask(clipboards: &Clipboards, sandbox: &str, request: &[u8]) -> Vec<u8> {
        let mut stream = connect(clipboards, sandbox).await.unwrap();
        stream.write_all(request).await.unwrap();
        stream.shutdown().await.unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.unwrap();
        answer
    }

    #[tokio::test]
    async fn answers_types_text_and_png() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeClipboard::with(Some(b"hello from the host"), Some(b"\x89PNG"));
        let clipboards = clipboards(dir.path(), host);
        clipboards.listen("sb").await;

        assert_eq!(
            ask(&clipboards, "sb", b"types\n").await,
            b"ok 9\ntext\npng\n"
        );
        assert_eq!(
            ask(&clipboards, "sb", b"text\n").await,
            b"ok 19\nhello from the host"
        );
        assert_eq!(ask(&clipboards, "sb", b"png\n").await, b"ok 4\n\x89PNG");
    }

    #[tokio::test]
    async fn answers_empty_for_a_kind_the_clipboard_lacks() {
        let dir = tempfile::tempdir().unwrap();
        let clipboards = clipboards(dir.path(), FakeClipboard::with(None, None));
        clipboards.listen("sb").await;

        assert_eq!(ask(&clipboards, "sb", b"types\n").await, b"ok 0\n");
        assert_eq!(ask(&clipboards, "sb", b"text\n").await, b"empty\n");
        assert_eq!(ask(&clipboards, "sb", b"png\n").await, b"empty\n");
    }

    #[tokio::test]
    async fn answers_unavailable_when_the_host_clipboard_cant_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeClipboard {
            available: false,
            ..FakeClipboard::with(Some(b"text"), None)
        };
        let clipboards = clipboards(dir.path(), host);
        clipboards.listen("sb").await;

        assert_eq!(ask(&clipboards, "sb", b"types\n").await, b"unavailable\n");
        assert_eq!(ask(&clipboards, "sb", b"text\n").await, b"unavailable\n");
    }

    #[tokio::test]
    async fn refuses_content_over_the_limits_even_when_the_host_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        let text = vec![b'a'; MAX_TEXT + 1];
        let png = vec![0; MAX_PNG + 1];
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(&text), Some(&png)));
        clipboards.listen("sb").await;

        assert_eq!(ask(&clipboards, "sb", b"text\n").await, b"too-large\n");
        assert_eq!(ask(&clipboards, "sb", b"png\n").await, b"too-large\n");
    }

    #[tokio::test]
    async fn sends_content_up_to_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let text = vec![b'a'; MAX_TEXT];
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(&text), None));
        clipboards.listen("sb").await;

        let answer = ask(&clipboards, "sb", b"text\n").await;

        assert!(answer.starts_with(format!("ok {MAX_TEXT}\n").as_bytes()));
        assert_eq!(answer.len(), format!("ok {MAX_TEXT}\n").len() + MAX_TEXT);
    }

    #[tokio::test]
    async fn answers_invalid_to_unknown_or_unfinished_requests() {
        let dir = tempfile::tempdir().unwrap();
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(b"text"), None));
        clipboards.listen("sb").await;

        let answers = [
            ask(&clipboards, "sb", b"primary\n").await,
            ask(&clipboards, "sb", b"text").await,
            ask(&clipboards, "sb", b"").await,
        ];

        assert_eq!(answers, [b"invalid\n"; 3]);
    }

    #[tokio::test]
    async fn sends_no_content_for_a_long_request() {
        let dir = tempfile::tempdir().unwrap();
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(b"text"), None));
        clipboards.listen("sb").await;
        let mut stream = connect(&clipboards, "sb").await.unwrap();
        let long = format!("text{}\n", " ".repeat(MAX_REQUEST * 4));

        stream.write_all(long.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        // fbkd closes the connection with the rest of the request unread, which may reset it.
        let read = stream.read_to_end(&mut answer).await;

        assert!(read.is_err() || answer == b"invalid\n", "{answer:?}");
    }

    /// Opens the connections and sends a request for text on each.
    async fn send_text_requests(clipboards: &Clipboards, count: usize) -> Vec<UnixStream> {
        let mut streams = Vec::new();

        for _ in 0..count {
            let mut stream = connect(clipboards, "sb").await.unwrap();
            stream.write_all(b"text\n").await.unwrap();
            streams.push(stream);
        }

        streams
    }

    #[tokio::test]
    async fn closes_connections_beyond_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeClipboard {
            gate: Some(tokio::sync::Semaphore::new(0)),
            ..FakeClipboard::with(Some(b"text"), None)
        };
        let host = Arc::new(host);
        let clipboards = Clipboards::new(Some(dir.path().join("clipboard")), host.clone());
        clipboards.listen("sb").await;
        let mut busy = send_text_requests(&clipboards, MAX_CONNECTIONS).await;
        // Let the listener accept the busy connections before the extra one.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut extra = connect(&clipboards, "sb").await.unwrap();
        let mut answer = Vec::new();
        let closed = tokio::time::timeout(Duration::from_secs(2), extra.read_to_end(&mut answer))
            .await
            .unwrap();
        host.gate.as_ref().unwrap().add_permits(MAX_CONNECTIONS);
        let mut first = Vec::new();
        busy[0].read_to_end(&mut first).await.unwrap();

        assert!(closed.is_err() || answer.is_empty(), "{answer:?}");
        assert_eq!(first, b"ok 4\ntext");
    }

    #[tokio::test]
    async fn remove_stops_listening_and_deletes_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(b"text"), None));
        clipboards.listen("sb").await;

        clipboards.remove("sb").await;

        assert!(!clipboards.socket_path("sb").unwrap().exists());
        assert!(connect(&clipboards, "sb").await.is_err());
    }

    #[tokio::test]
    async fn add_route_adds_the_marker_and_a_stream_route_to_the_socket() {
        let clipboards = Clipboards::new(
            Some(PathBuf::from("/run/firebrick/clipboard")),
            Arc::new(FakeClipboard::with(None, None)),
        );
        let builder = clipboards.add_route(
            microsandbox::Sandbox::builder("sb").image("ubuntu:26.04"),
            "sb",
        );
        let spec = builder.build().await.unwrap().spec;

        assert!(clipboards.has_route("sb", &spec));
        assert!(!clipboards.has_route("other", &spec));
        assert!(spec.runtime.scripts.contains_key(MARKER));
    }

    #[tokio::test]
    async fn add_route_without_a_socket_path_adds_only_the_marker() {
        let clipboards = Clipboards::new(None, Arc::new(FakeClipboard::with(None, None)));
        let builder = clipboards.add_route(
            microsandbox::Sandbox::builder("sb").image("ubuntu:26.04"),
            "sb",
        );
        let spec = builder.build().await.unwrap().spec;

        assert!(spec.vsock.routes.is_empty());
        assert!(spec.runtime.scripts.contains_key(MARKER));
    }

    #[test]
    fn is_enabled_reads_the_label() {
        let labels = |value: &str| BTreeMap::from([(ENABLED_LABEL.to_string(), value.into())]);

        assert!(is_enabled(&labels("true")));
        assert!(!is_enabled(&labels("false")));
        assert!(!is_enabled(&BTreeMap::new()));
    }

    #[test]
    fn kinds_from_types_recognizes_text_and_png() {
        assert_eq!(
            kinds_from_types("image/png\ntext/plain;charset=utf-8\n"),
            [Kind::Text, Kind::Png]
        );
        assert_eq!(
            kinds_from_types("TARGETS\nUTF8_STRING\nSTRING\n"),
            [Kind::Text]
        );
        assert_eq!(kinds_from_types("image/jpeg\ntext/html\n"), []);
    }

    #[test]
    fn kinds_from_clipboard_info_recognizes_text_and_png() {
        assert_eq!(
            kinds_from_clipboard_info("«class PNGf», 1234, «class utf8», 5, string, 5"),
            [Kind::Text, Kind::Png]
        );
        assert_eq!(
            kinds_from_clipboard_info("«class PNGf», 1234, «class 8BPS», 99"),
            [Kind::Png]
        );
        assert_eq!(kinds_from_clipboard_info(""), []);
    }

    #[test]
    fn png_from_applescript_decodes_the_hex_data() {
        assert_eq!(
            png_from_applescript("«data PNGf89504E47»\n"),
            Some(vec![0x89, 0x50, 0x4e, 0x47])
        );
        assert_eq!(png_from_applescript("«data PNGf895»"), None);
        assert_eq!(png_from_applescript("«data PNGfZZ»"), None);
        assert_eq!(png_from_applescript("text"), None);
    }

    #[test]
    fn backend_follows_the_platform_and_the_display_variables() {
        let set = |names: &'static [&'static str]| move |name: &str| names.contains(&name);

        assert_eq!(Backend::detect(true, set(&[])), Ok(Backend::MacOs));
        assert_eq!(
            Backend::detect(false, set(&["WAYLAND_DISPLAY", "DISPLAY"])),
            Ok(Backend::Wayland)
        );
        assert_eq!(Backend::detect(false, set(&["DISPLAY"])), Ok(Backend::X11));
        assert!(matches!(
            Backend::detect(false, set(&[])),
            Err(HostError::Unavailable(_))
        ));
    }

    #[test]
    fn a_tool_that_reports_nothing_copied_means_an_empty_clipboard() {
        let empty = Backend::Wayland.empty_or_unavailable::<()>("wl-paste", "Nothing is copied\n");
        let x11 = Backend::X11
            .empty_or_unavailable::<()>("xclip", "Error: target TARGETS not available\n");
        let broken = Backend::Wayland
            .empty_or_unavailable::<()>("wl-paste", "Failed to connect to a Wayland server\n");

        assert_eq!(empty, Err(HostError::Empty));
        assert_eq!(x11, Err(HostError::Empty));
        assert_eq!(
            broken,
            Err(HostError::Unavailable(
                "wl-paste failed: Failed to connect to a Wayland server".to_string()
            ))
        );
    }

    // `run` itself isn't tested here: spawning processes in the unit tests races with the tests
    // in `forward`, which expect the port of a closed listener to be free at once, while a
    // forked child may still hold it. The smoke test reads the real host clipboard.
    #[tokio::test]
    async fn read_at_most_stops_at_the_limit() {
        let output = [0; 100];

        let read = read_at_most(Some(&output[..]), 11).await.unwrap();
        let none = read_at_most(None::<&[u8]>, 11).await.unwrap();

        assert_eq!(read.len(), 11);
        assert!(none.is_empty());
    }
}
