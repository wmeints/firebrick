//! Shares the host clipboard with the sandboxes created with `clipboard: true`, both ways.
//!
//! The `wl-paste`, `wl-copy` and `xclip` stand-ins in the `firebrick-base` image connect to
//! vsock port [`GUEST_PORT`] on the host. Sandboxes with the [`ENABLED_LABEL`] get a stream route from that
//! port to their own unix socket in [`firebrick_utils::clipboard_socket_dir`], where fbkd
//! listens; other sandboxes get no route, so nothing in their guest reaches the clipboard, not
//! even root. The label also gives the guest the [`MARKER`] script, so the stand-ins can tell a
//! sandbox without the setting from one whose fbkd doesn't answer.
//!
//! Each connection carries one request line, `types`, `text`, `png`, or `copy text <length>` or
//! `copy png <length>` followed by that many bytes of content, and gets one answer: `ok
//! <length>` followed by that many bytes, or one of `empty`, `too-large`, `unavailable <cause>`,
//! `invalid`, `bad-content`, `busy` and `denied`. fbkd reads and writes the host clipboard
//! through a [`HostClipboard`] only when asked, and never sends or sets more than [`MAX_TEXT`]
//! bytes of text or [`MAX_PNG`] bytes of PNG.
//!
//! Only sandboxes with the [`COPY_LABEL`] can copy, so sandboxes created before copying existed,
//! whose users approved pasting only, stay paste-only until they're recreated. fbkd sets the
//! clipboard for one sandbox at most [`MAX_COPIES`] times within [`COPY_WINDOW`].

use crate::rate_limit::RateLimit;
use crate::vsock::{self, Connections, Listeners, Refused};
use async_trait::async_trait;
use microsandbox::sandbox::{SandboxBuilder, SandboxSpec};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

/// Guest vsock port the stand-ins connect to, on host CID 2.
pub const GUEST_PORT: u32 = 52001;

/// Sandbox label that records whether the sandbox shares the host clipboard. Only sandboxes
/// created with the setting have it, set to `true`.
pub const ENABLED_LABEL: &str = "firebrick.clipboard";

/// Sandbox label that records whether the sandbox can also write the host clipboard. Sandboxes
/// created with the setting since copying exists have it, set to `true`.
pub const COPY_LABEL: &str = "firebrick.clipboard.copy";

/// Name of the microsandbox script that marks a sandbox with the setting. microsandbox puts it
/// in the guest at `/.msb/scripts/<name>`.
pub const MARKER: &str = "firebrick-clipboard";

/// Content of the [`MARKER`] script.
const MARKER_SCRIPT: &str = "#!/bin/sh\n\
    # fbkd shares the host clipboard with this sandbox on vsock port 52001.\n\
    echo 'clipboard sharing is on for this sandbox'\n";

/// The name of the channel in log lines.
const CHANNEL: &str = "clipboard";

/// The most bytes of text fbkd sends or sets.
pub const MAX_TEXT: usize = 1024 * 1024;

/// The most bytes of PNG fbkd sends or sets.
pub const MAX_PNG: usize = 10 * 1024 * 1024;

/// The most copies fbkd takes from one sandbox within [`COPY_WINDOW`].
pub const MAX_COPIES: usize = 10;

/// The window [`MAX_COPIES`] applies to.
pub const COPY_WINDOW: Duration = Duration::from_secs(10);

/// The bytes every PNG starts with.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The longest request line fbkd reads, in bytes, without its newline.
const MAX_REQUEST: usize = 32;

/// How long a stand-in gets to send its request, and then the content of a copy.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long reading or writing the host clipboard may take. The stand-ins give up after 5
/// seconds.
const HOST_TIMEOUT: Duration = Duration::from_secs(4);

/// How often fbkd logs a warning about one sandbox's clipboard at most.
const WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// The most bytes of a host tool's stderr fbkd keeps to log.
const MAX_STDERR: usize = 1024;

/// How long fbkd reads the stderr of a copy tool that failed.
const STDERR_TIMEOUT: Duration = Duration::from_millis(500);

/// The most bytes of the list of types fbkd reads.
const MAX_TYPES: usize = 64 * 1024;

/// Returns whether a sandbox with the labels shares the host clipboard.
pub fn is_enabled(labels: &BTreeMap<String, String>) -> bool {
    labels.get(ENABLED_LABEL).map(String::as_str) == Some("true")
}

/// Returns whether a sandbox with the labels can also write the host clipboard.
pub fn can_copy(labels: &BTreeMap<String, String>) -> bool {
    is_enabled(labels) && labels.get(COPY_LABEL).map(String::as_str) == Some("true")
}

/// A kind of clipboard content the sandbox can read and write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// UTF-8 text.
    Text,
    /// A PNG image.
    Png,
}

impl Kind {
    /// Returns the most bytes of the kind fbkd sends or sets.
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

    /// Returns the kind with the name in the protocol.
    fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"text" => Some(Kind::Text),
            b"png" => Some(Kind::Png),
            _ => None,
        }
    }

    /// Returns whether the content is valid for the kind: UTF-8 text, or data that starts with
    /// the PNG signature.
    fn is_valid(self, content: &[u8]) -> bool {
        match self {
            Kind::Text => std::str::from_utf8(content).is_ok(),
            Kind::Png => content.starts_with(PNG_SIGNATURE),
        }
    }
}

/// Why the host clipboard can't be read, as the stand-ins learn it, so they can give a hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// fbkd has no display to read the clipboard of.
    NoDisplay,
    /// The host has no clipboard tool for the desktop.
    NoTool,
    /// The clipboard tool failed or took too long.
    Failed,
}

impl Cause {
    /// Returns the name of the cause in the protocol.
    fn name(self) -> &'static str {
        match self {
            Cause::NoDisplay => "no-display",
            Cause::NoTool => "no-tool",
            Cause::Failed => "failed",
        }
    }
}

/// Why the host clipboard can't give or take what was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// The clipboard is empty or doesn't hold the kind.
    #[error("the clipboard holds no such content")]
    Empty,
    /// The content is larger than the kind's limit.
    #[error("the clipboard content is too large")]
    TooLarge,
    /// The clipboard can't be read or written. The reason is only logged, never sent to the
    /// sandbox.
    #[error("{1}")]
    Unavailable(Cause, String),
}

/// Reads and writes the clipboard of the host.
#[async_trait]
pub trait HostClipboard: Send + Sync {
    /// Returns the kinds the clipboard holds, empty when it's empty.
    async fn kinds(&self) -> Result<Vec<Kind>, HostError>;

    /// Returns the content of the kind, failing with [`HostError::TooLarge`] without reading
    /// much more than `limit` bytes when it's larger.
    async fn read(&self, kind: Kind, limit: usize) -> Result<Vec<u8>, HostError>;

    /// Replaces the clipboard with the content of the kind. fbkd checked that the content is
    /// valid for the kind and within its limit.
    async fn write(&self, kind: Kind, content: Vec<u8>) -> Result<(), HostError>;
}

/// A request from a stand-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    /// The kinds the clipboard holds.
    Types,
    /// The content of a kind.
    Read(Kind),
    /// Sets the clipboard to the content of a kind, of the length, that follows the request.
    Write(Kind, usize),
}

/// Parses a request line without its newline.
fn parse_request(line: &[u8]) -> Option<Request> {
    match line {
        b"types" => Some(Request::Types),
        _ => Kind::from_name(line)
            .map(Request::Read)
            .or_else(|| parse_copy(line)),
    }
}

/// Parses a `copy <kind> <length>` request line.
fn parse_copy(line: &[u8]) -> Option<Request> {
    let mut words = line.strip_prefix(b"copy ")?.split(|byte| *byte == b' ');
    let kind = Kind::from_name(words.next()?)?;
    let length = words.next()?;

    if words.next().is_some() || length.is_empty() || !length.iter().all(u8::is_ascii_digit) {
        return None;
    }

    let length = std::str::from_utf8(length).ok()?.parse().ok()?;

    Some(Request::Write(kind, length))
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
    Unavailable(Cause),
    /// The request isn't one fbkd knows.
    Invalid,
    /// The content to copy isn't valid for its kind.
    BadContent,
    /// The sandbox copied too often lately.
    Busy,
    /// The sandbox can't write the host clipboard.
    Denied,
}

impl Reply {
    /// Returns the reply as it's sent: a status line, followed by the content.
    fn encode(self) -> Vec<u8> {
        let status = match &self {
            Reply::Content(content) => format!("ok {}\n", content.len()),
            Reply::Empty => "empty\n".to_string(),
            Reply::TooLarge => "too-large\n".to_string(),
            Reply::Unavailable(cause) => format!("unavailable {}\n", cause.name()),
            Reply::Invalid => "invalid\n".to_string(),
            Reply::BadContent => "bad-content\n".to_string(),
            Reply::Busy => "busy\n".to_string(),
            Reply::Denied => "denied\n".to_string(),
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
    /// Whether the sandbox can write the host clipboard.
    copy: bool,
    /// The copies the sandbox made lately.
    copies: Mutex<RateLimit>,
    /// When fbkd last logged a warning about the sandbox's clipboard.
    last_warning: Mutex<Option<Instant>>,
}

impl Channel {
    /// Answers a request from the sandbox, reading the content of a copy from `content`.
    /// Returns `None` when the sandbox doesn't send all of that content in time.
    async fn answer(&self, request: Request, content: impl AsyncRead + Unpin) -> Option<Reply> {
        let result = match request {
            Request::Types => self.host.kinds().await.map(|kinds| kind_list(&kinds)),
            Request::Read(kind) => self.read(kind).await,
            Request::Write(kind, length) => return self.copy(kind, length, content).await,
        };

        Some(self.reply(result))
    }

    /// Sets the host clipboard to the `length` bytes of the kind that `content` holds, when the
    /// sandbox may copy, hasn't copied too often lately, and the content is within the kind's
    /// limit and valid for it. Returns `None` when the sandbox doesn't send all of it in time.
    async fn copy(
        &self,
        kind: Kind,
        length: usize,
        content: impl AsyncRead + Unpin,
    ) -> Option<Reply> {
        if !self.copy {
            return Some(Reply::Denied);
        }

        if !self.allow_copy() {
            self.warn(&format!(
                "refused copies beyond the limit of {MAX_COPIES} within {COPY_WINDOW:?}"
            ));
            return Some(Reply::Busy);
        }

        if length > kind.limit() {
            return Some(Reply::TooLarge);
        }

        let content =
            match tokio::time::timeout(REQUEST_TIMEOUT, read_content(content, length)).await {
                Ok(Ok(content)) => content,
                Ok(Err(err)) => return self.unfinished(kind, length, &err.to_string()),
                Err(_) => return self.unfinished(kind, length, "it took too long"),
            };

        if !kind.is_valid(&content) {
            return Some(Reply::BadContent);
        }

        let written = self.host.write(kind, content).await;

        Some(self.reply(written.map(|()| Vec::new())))
    }

    /// Logs a copy whose content didn't all arrive, and returns `None`: there's nobody to
    /// answer.
    fn unfinished(&self, kind: Kind, length: usize, reason: &str) -> Option<Reply> {
        tracing::debug!(
            "sandbox {} didn't send the {length} bytes of {} it copies: {reason}",
            self.sandbox,
            kind.name()
        );

        None
    }

    /// Records a copy now and returns whether the rate limit allows it.
    fn allow_copy(&self) -> bool {
        self.copies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .allow(Instant::now())
    }

    /// Returns the reply for the result of using the host clipboard, logging why it isn't
    /// available.
    fn reply(&self, result: Result<Vec<u8>, HostError>) -> Reply {
        match result {
            Ok(content) => Reply::Content(content),
            Err(HostError::Empty) => Reply::Empty,
            Err(HostError::TooLarge) => Reply::TooLarge,
            Err(HostError::Unavailable(cause, reason)) => {
                self.warn(&format!("the host clipboard isn't available: {reason}"));
                Reply::Unavailable(cause)
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
    let mut read = BufReader::new(read);
    let request = tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut read)).await;
    let reply = match request {
        Ok(Ok(Some(request))) => channel.answer(request, read).await,
        Ok(Ok(None)) => Some(Reply::Invalid),
        // The stand-in went away or took too long; there's nobody to answer.
        Ok(Err(_)) | Err(_) => None,
    };
    let Some(reply) = reply else {
        return;
    };

    if let Err(err) = write.write_all(&reply.encode()).await {
        tracing::debug!(
            "failed to answer a clipboard request of sandbox {}: {err}",
            channel.sandbox
        );
    }
}

/// Reads the request line, at most [`MAX_REQUEST`] bytes of it, and leaves what follows it in
/// `read`. Returns `None` when it isn't a known request.
async fn read_request(read: &mut (impl AsyncBufRead + Unpin)) -> io::Result<Option<Request>> {
    let limit = u64::try_from(MAX_REQUEST + 1).unwrap_or(u64::MAX);
    let mut line = Vec::with_capacity(MAX_REQUEST + 1);
    read.take(limit).read_until(b'\n', &mut line).await?;

    Ok(line.strip_suffix(b"\n").and_then(parse_request))
}

/// Reads exactly `length` bytes of content, failing when the stream ends before.
async fn read_content(read: impl AsyncRead + Unpin, length: usize) -> io::Result<Vec<u8>> {
    let content = read_at_most(Some(read), length).await?;

    if content.len() < length {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }

    Ok(content)
}

/// Answers an accepted connection, or closes it with a warning when the sandbox already has
/// [`vsock::MAX_CONNECTIONS`] connections.
fn accept(
    connections: &mut Connections,
    accepted: io::Result<(UnixStream, tokio::net::unix::SocketAddr)>,
    channel: &Arc<Channel>,
) {
    let answer = |stream| serve_connection(stream, Arc::clone(channel));

    match connections.accept(accepted, answer) {
        Ok(()) => {}
        Err(Refused::Full) => channel.warn(&format!(
            "closed a connection beyond the limit of {}",
            vsock::MAX_CONNECTIONS
        )),
        Err(Refused::Failed(err)) => channel.warn(&format!("failed to accept a connection: {err}")),
    }
}

/// Logs a connection task that panicked.
fn finished(channel: &Channel, result: Result<(), tokio::task::JoinError>) {
    if let Some(err) = vsock::panicked(result) {
        tracing::error!(
            "a clipboard connection of sandbox {} failed: {err}",
            channel.sandbox
        );
    }
}

/// Accepts connections from the sandbox until `shutdown` fires or its sender is dropped, and
/// answers them.
async fn serve(listener: UnixListener, channel: Arc<Channel>, shutdown: oneshot::Receiver<()>) {
    let mut connections = Connections::default();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => accept(&mut connections, accepted, &channel),
            Some(result) = connections.join_next() => finished(&channel, result),
            _ = &mut shutdown => return,
        }
    }
}

/// Listens for the clipboard requests of the sandboxes that share the host clipboard.
pub struct Clipboards {
    listeners: Listeners<Channel>,
    host: Arc<dyn HostClipboard>,
}

impl Clipboards {
    /// Creates a registry with the sockets in `socket_dir` that reads the clipboard from
    /// `host`. Without a socket directory, sandboxes get no route.
    pub fn new(socket_dir: Option<PathBuf>, host: Arc<dyn HostClipboard>) -> Self {
        Self {
            listeners: Listeners::new(CHANNEL, GUEST_PORT, socket_dir),
            host,
        }
    }

    /// Creates a registry with the sockets in the directory the environment selects, see
    /// [`firebrick_utils::clipboard_socket_dir`]. Leaves out a directory that isn't private to
    /// the user, with a warning.
    pub fn from_env(host: Arc<dyn HostClipboard>) -> Self {
        let dir = firebrick_utils::clipboard_socket_dir();

        Self {
            listeners: Listeners::private(CHANNEL, GUEST_PORT, dir),
            host,
        }
    }

    /// Returns the socket of the sandbox, or `None` when there's no socket directory or the
    /// path is too long for a unix socket.
    pub fn socket_path(&self, sandbox: &str) -> Option<PathBuf> {
        self.listeners.socket_path(sandbox)
    }

    /// Adds the [`MARKER`] script and the route from [`GUEST_PORT`] to the sandbox's socket to
    /// the builder. Leaves out the route, with a warning, when the sandbox has no socket path,
    /// since microsandbox would then refuse to boot the sandbox.
    pub fn add_route(&self, builder: SandboxBuilder, sandbox: &str) -> SandboxBuilder {
        let builder = builder.script(MARKER, MARKER_SCRIPT);

        self.listeners.add_route(builder, sandbox)
    }

    /// Whether the sandbox has a stream route from [`GUEST_PORT`] to its socket.
    pub fn has_route(&self, sandbox: &str, spec: &SandboxSpec) -> bool {
        self.listeners.has_route(sandbox, spec)
    }

    /// Listens on the sandbox's socket unless fbkd already does, and lets the sandbox write the
    /// host clipboard when `copy` is set. A listener that stopped is replaced. Fails when the
    /// sandbox has no socket path, or the socket can't be bound.
    pub async fn listen(&self, sandbox: &str, copy: bool) -> io::Result<()> {
        let channel = Channel {
            sandbox: sandbox.to_string(),
            host: Arc::clone(&self.host),
            copy,
            copies: Mutex::new(RateLimit::new(MAX_COPIES, COPY_WINDOW)),
            last_warning: Mutex::new(None),
        };

        self.listeners
            .listen(sandbox, async { channel }, serve)
            .await
    }

    /// Stops listening for the sandbox and deletes its socket.
    pub async fn remove(&self, sandbox: &str) {
        self.listeners.remove(sandbox, |_| {}).await;
    }

    /// Stops listening for every sandbox. Run when the daemon shuts down.
    pub async fn close(&self) {
        self.listeners.close().await;
    }
}

/// A desktop whose clipboard tools [`HostTools`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    /// A Wayland session, read with `wl-paste` and written with `wl-copy`.
    Wayland,
    /// An X11 session, read and written with `xclip`.
    X11,
    /// macOS, read with `pbpaste` and `osascript` and written with `osascript`.
    MacOs,
}

/// Reads and writes the host clipboard with the tools of the desktop: `wl-paste` and `wl-copy`
/// on Wayland, `xclip` on X11, and `pbpaste` and `osascript` on macOS. fbkd inherits the
/// display of the `fbk` that started it, so it uses the clipboard of that desktop session.
#[derive(Debug, Clone)]
pub struct HostTools {
    desktop: Option<Desktop>,
    path: Option<OsString>,
}

impl HostTools {
    /// Returns the tools of the desktop fbkd runs on: macOS, or on Linux Wayland when
    /// `WAYLAND_DISPLAY` is set and X11 when `DISPLAY` is. They're looked up on fbkd's `PATH`.
    pub fn from_env() -> Self {
        let is_set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());

        Self::new(detect(cfg!(target_os = "macos"), is_set), None)
    }

    /// Returns the tools of the desktop, or of none when there's no display, looked up on
    /// `path` instead of fbkd's `PATH` when it's given.
    pub fn new(desktop: Option<Desktop>, path: Option<OsString>) -> Self {
        Self { desktop, path }
    }

    /// Returns the desktop, failing when there's no display.
    fn desktop(&self) -> Result<Desktop, HostError> {
        self.desktop.ok_or_else(|| {
            HostError::Unavailable(
                Cause::NoDisplay,
                "neither WAYLAND_DISPLAY nor DISPLAY is set; start fbkd from your desktop \
                 session by running fbk there first"
                    .to_string(),
            )
        })
    }

    /// Runs the tool, failing when it reports an error.
    async fn run(&self, (program, args): Command, limit: usize) -> Result<Vec<u8>, HostError> {
        let output = run(program, args, self.path.as_ref(), limit).await?;

        if !output.success {
            return Err(failure(program, &output.stderr));
        }

        Ok(output.stdout)
    }
}

#[async_trait]
impl HostClipboard for HostTools {
    async fn kinds(&self) -> Result<Vec<Kind>, HostError> {
        let desktop = self.desktop()?;
        let types = match self.run(list_command(desktop), MAX_TYPES).await {
            Err(HostError::Empty) => return Ok(Vec::new()),
            types => types?,
        };
        let types = String::from_utf8_lossy(&types);

        Ok(match desktop {
            Desktop::MacOs => kinds_from_clipboard_info(&types),
            Desktop::Wayland | Desktop::X11 => kinds_from_types(&types),
        })
    }

    async fn read(&self, kind: Kind, limit: usize) -> Result<Vec<u8>, HostError> {
        if !self.kinds().await?.contains(&kind) {
            return Err(HostError::Empty);
        }

        let desktop = self.desktop()?;
        let output_limit = output_limit(desktop, kind, limit);
        let output = self.run(read_command(desktop, kind), output_limit).await?;

        content(desktop, kind, output, limit)
    }

    async fn write(&self, kind: Kind, content: Vec<u8>) -> Result<(), HostError> {
        let desktop = self.desktop()?;
        let (program, args) = write_command(desktop, kind);

        run_with_input(
            program,
            args,
            self.path.as_ref(),
            input(desktop, kind, content),
        )
        .await
    }
}

/// Returns the desktop for macOS when `macos` is set, and otherwise by the display variables
/// that `is_set` reports, or `None` when there's no display.
fn detect(macos: bool, is_set: impl Fn(&str) -> bool) -> Option<Desktop> {
    if macos {
        Some(Desktop::MacOs)
    } else if is_set("WAYLAND_DISPLAY") {
        Some(Desktop::Wayland)
    } else if is_set("DISPLAY") {
        Some(Desktop::X11)
    } else {
        None
    }
}

/// A program and its arguments.
type Command = (&'static str, &'static [&'static str]);

/// Returns the command that lists the types on the clipboard.
fn list_command(desktop: Desktop) -> Command {
    match desktop {
        Desktop::Wayland => ("wl-paste", &["--list-types"]),
        Desktop::X11 => ("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]),
        Desktop::MacOs => ("osascript", &["-e", "clipboard info"]),
    }
}

/// Returns the command that prints the content of the kind.
fn read_command(desktop: Desktop, kind: Kind) -> Command {
    match (desktop, kind) {
        (Desktop::Wayland, Kind::Text) => ("wl-paste", &["--no-newline", "--type", "text"]),
        (Desktop::Wayland, Kind::Png) => ("wl-paste", &["--type", "image/png"]),
        (Desktop::X11, Kind::Text) => (
            "xclip",
            &["-selection", "clipboard", "-t", "UTF8_STRING", "-o"],
        ),
        (Desktop::X11, Kind::Png) => (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
        (Desktop::MacOs, Kind::Text) => ("pbpaste", &[]),
        (Desktop::MacOs, Kind::Png) => ("osascript", &["-e", "the clipboard as «class PNGf»"]),
    }
}

/// Returns the command that sets the clipboard to the content of the kind on its stdin.
fn write_command(desktop: Desktop, kind: Kind) -> Command {
    match (desktop, kind) {
        (Desktop::Wayland, Kind::Text) => ("wl-copy", &["--type", "text/plain;charset=utf-8"]),
        (Desktop::Wayland, Kind::Png) => ("wl-copy", &["--type", "image/png"]),
        (Desktop::X11, Kind::Text) => (
            "xclip",
            &["-selection", "clipboard", "-t", "UTF8_STRING", "-i"],
        ),
        (Desktop::X11, Kind::Png) => (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-i"],
        ),
        // osascript reads the script from stdin.
        (Desktop::MacOs, _) => ("osascript", &["-"]),
    }
}

/// Returns the stdin of the [`write_command`] for the content: the content itself, or on macOS
/// an AppleScript that sets the clipboard to it as hex data of the kind's class.
fn input(desktop: Desktop, kind: Kind, content: Vec<u8>) -> Vec<u8> {
    if desktop != Desktop::MacOs {
        return content;
    }

    let class = match kind {
        Kind::Text => "utf8",
        Kind::Png => "PNGf",
    };
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut script = format!("set the clipboard to «data {class}").into_bytes();
    script.reserve(content.len() * 2 + 8);

    for byte in content {
        script.push(DIGITS[usize::from(byte >> 4)]);
        script.push(DIGITS[usize::from(byte & 0x0f)]);
    }

    script.extend_from_slice("»\n".as_bytes());
    script
}

/// Returns the most bytes of output to read for content of at most `limit` bytes. AppleScript
/// prints PNG data as hex.
fn output_limit(desktop: Desktop, kind: Kind, limit: usize) -> usize {
    match (desktop, kind) {
        (Desktop::MacOs, Kind::Png) => limit.saturating_mul(2).saturating_add(64),
        _ => limit,
    }
}

/// Returns the content from the command's output.
fn content(
    desktop: Desktop,
    kind: Kind,
    output: Vec<u8>,
    limit: usize,
) -> Result<Vec<u8>, HostError> {
    let content = match (desktop, kind) {
        (Desktop::MacOs, Kind::Png) => png_from_applescript(&String::from_utf8_lossy(&output))
            .ok_or_else(|| {
                HostError::Unavailable(Cause::Failed, "osascript printed no PNG data".to_string())
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
fn failure(program: &str, stderr: &str) -> HostError {
    let empty = ["Nothing is copied", "No selection", "not available"]
        .iter()
        .any(|message| stderr.contains(message));

    if empty {
        return HostError::Empty;
    }

    HostError::Unavailable(
        Cause::Failed,
        format!(
            "{program} failed: {}",
            firebrick_utils::strip_control(stderr.trim())
        ),
    )
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

/// Runs the program, looked up on `path` when it's given, and returns its output. Fails with
/// [`HostError::TooLarge`] as soon as it prints more than `limit` bytes, and with
/// [`HostError::Unavailable`] when it can't run or takes longer than [`HOST_TIMEOUT`]. The
/// program is killed when it's given up on.
async fn run(
    program: &str,
    args: &[&str],
    path: Option<&OsString>,
    limit: usize,
) -> Result<Output, HostError> {
    let mut command = command(program, args, path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command.spawn().map_err(|err| spawn_error(program, &err))?;

    tokio::time::timeout(HOST_TIMEOUT, collect(program, child, limit))
        .await
        .map_err(|_| timed_out(program))?
}

/// Runs the program, looked up on `path` when it's given, with the input on its stdin, and
/// waits for it to exit. Fails with [`HostError::Unavailable`] when it can't run, fails, or
/// takes longer than [`HOST_TIMEOUT`], and kills it when it's given up on. Its stdout goes
/// nowhere, and its stderr is only read when it fails: `wl-copy` and `xclip` leave a child
/// behind that serves the clipboard and keeps the pipes it inherited open.
async fn run_with_input(
    program: &str,
    args: &[&str],
    path: Option<&OsString>,
    input: Vec<u8>,
) -> Result<(), HostError> {
    let mut command = command(program, args, path);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let child = command.spawn().map_err(|err| spawn_error(program, &err))?;

    tokio::time::timeout(HOST_TIMEOUT, feed(program, child, input))
        .await
        .map_err(|_| timed_out(program))?
}

/// Returns the command for the program, looked up on `path` when it's given, which kills the
/// program when its child is dropped.
fn command(program: &str, args: &[&str], path: Option<&OsString>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command.args(args).kill_on_drop(true);

    // A PATH set for the child is also where the program is looked up.
    if let Some(path) = path {
        command.env("PATH", path);
    }

    command
}

/// Returns the error for a program that took longer than [`HOST_TIMEOUT`].
fn timed_out(program: &str) -> HostError {
    let reason = format!("{program} didn't finish within {HOST_TIMEOUT:?}");

    HostError::Unavailable(Cause::Failed, reason)
}

/// Writes the input to the stdin of the program's child, closes it and waits for the child to
/// exit, failing with what it printed on stderr when it doesn't exit successfully.
async fn feed(
    program: &str,
    mut child: tokio::process::Child,
    input: Vec<u8>,
) -> Result<(), HostError> {
    let failed =
        |reason: String| HostError::Unavailable(Cause::Failed, format!("{program} {reason}"));
    let written = match child.stdin.take() {
        // Dropping stdin once the write is done closes it.
        Some(mut stdin) => stdin.write_all(&input).await,
        None => Ok(()),
    };
    let status = child
        .wait()
        .await
        .map_err(|err| failed(format!("failed: {err}")))?;

    if !status.success() {
        let stderr = failure_stderr(child.stderr.take()).await;
        return Err(failed(format!("failed ({status}): {stderr}")));
    }

    written.map_err(|err| failed(format!("can't be written to: {err}")))
}

/// Returns at most [`MAX_STDERR`] bytes of what a program that failed printed on stderr,
/// without control characters. Gives up after [`STDERR_TIMEOUT`], in case a child it left
/// behind keeps the pipe open.
async fn failure_stderr(stderr: Option<tokio::process::ChildStderr>) -> String {
    let read = tokio::time::timeout(STDERR_TIMEOUT, read_at_most(stderr, MAX_STDERR)).await;
    let stderr = match read {
        Ok(Ok(stderr)) => String::from_utf8_lossy(&stderr).into_owned(),
        Ok(Err(err)) => format!("(its stderr can't be read: {err})"),
        Err(_) => "(its stderr didn't end)".to_string(),
    };

    firebrick_utils::strip_control(stderr.trim())
}

/// Returns the error for a program that couldn't be started: a missing tool, or a failure.
fn spawn_error(program: &str, err: &io::Error) -> HostError {
    let cause = match err.kind() {
        io::ErrorKind::NotFound => Cause::NoTool,
        _ => Cause::Failed,
    };

    HostError::Unavailable(cause, format!("{program} can't run: {err}"))
}

/// Reads the output of the program's child and waits for it to exit, failing with
/// [`HostError::TooLarge`] as soon as it prints more than `limit` bytes.
async fn collect(
    program: &str,
    mut child: tokio::process::Child,
    limit: usize,
) -> Result<Output, HostError> {
    let failed =
        |reason: String| HostError::Unavailable(Cause::Failed, format!("{program} {reason}"));
    let (stdout, stderr) = tokio::join!(
        read_at_most(child.stdout.take(), limit.saturating_add(1)),
        read_at_most(child.stderr.take(), MAX_STDERR)
    );
    let stdout = stdout.map_err(|err| failed(format!("can't be read: {err}")))?;

    if stdout.len() > limit {
        return Err(HostError::TooLarge);
    }

    let status = child
        .wait()
        .await
        .map_err(|err| failed(format!("failed: {err}")))?;
    let stderr = match stderr {
        Ok(stderr) => String::from_utf8_lossy(&stderr).into_owned(),
        Err(err) => format!("(its stderr can't be read: {err})"),
    };

    Ok(Output {
        success: status.success(),
        stdout,
        stderr,
    })
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
        /// What was written to the clipboard, in order.
        written: Mutex<Vec<(Kind, Vec<u8>)>>,
    }

    impl FakeClipboard {
        fn with(text: Option<&[u8]>, png: Option<&[u8]>) -> Self {
            Self {
                text: text.map(<[u8]>::to_vec),
                png: png.map(<[u8]>::to_vec),
                available: true,
                gate: None,
                written: Mutex::new(Vec::new()),
            }
        }

        fn written(&self) -> Vec<(Kind, Vec<u8>)> {
            self.written.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HostClipboard for FakeClipboard {
        async fn kinds(&self) -> Result<Vec<Kind>, HostError> {
            if !self.available {
                let reason = "no display".to_string();
                return Err(HostError::Unavailable(Cause::NoDisplay, reason));
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

        async fn write(&self, kind: Kind, content: Vec<u8>) -> Result<(), HostError> {
            self.kinds().await?;
            self.written.lock().unwrap().push((kind, content));

            Ok(())
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
        clipboards.listen("sb", true).await.unwrap();

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
        clipboards.listen("sb", true).await.unwrap();

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
        clipboards.listen("sb", true).await.unwrap();

        let unavailable = b"unavailable no-display\n";
        assert_eq!(ask(&clipboards, "sb", b"types\n").await, unavailable);
        assert_eq!(ask(&clipboards, "sb", b"text\n").await, unavailable);
    }

    #[tokio::test]
    async fn refuses_content_over_the_limits_even_when_the_host_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        let text = vec![b'a'; MAX_TEXT + 1];
        let png = vec![0; MAX_PNG + 1];
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(&text), Some(&png)));
        clipboards.listen("sb", true).await.unwrap();

        assert_eq!(ask(&clipboards, "sb", b"text\n").await, b"too-large\n");
        assert_eq!(ask(&clipboards, "sb", b"png\n").await, b"too-large\n");
    }

    #[tokio::test]
    async fn sends_content_up_to_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let text = vec![b'a'; MAX_TEXT];
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(&text), None));
        clipboards.listen("sb", true).await.unwrap();

        let answer = ask(&clipboards, "sb", b"text\n").await;

        assert!(answer.starts_with(format!("ok {MAX_TEXT}\n").as_bytes()));
        assert_eq!(answer.len(), format!("ok {MAX_TEXT}\n").len() + MAX_TEXT);
    }

    #[tokio::test]
    async fn answers_invalid_to_unknown_or_unfinished_requests() {
        let dir = tempfile::tempdir().unwrap();
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(b"text"), None));
        clipboards.listen("sb", true).await.unwrap();

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
        clipboards.listen("sb", true).await.unwrap();
        let mut stream = connect(&clipboards, "sb").await.unwrap();
        let long = format!("text{}\n", " ".repeat(MAX_REQUEST * 4));

        stream.write_all(long.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        // fbkd closes the connection with the rest of the request unread, which may reset it,
        // so the read may fail; the bytes it got before that still count.
        let _ = stream.read_to_end(&mut answer).await;

        assert!(answer.is_empty() || answer == b"invalid\n", "{answer:?}");
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
        clipboards.listen("sb", true).await.unwrap();
        // The listener accepts connections in the order they connect, so it has accepted the
        // busy ones by the time it accepts the extra one.
        let mut busy = send_text_requests(&clipboards, vsock::MAX_CONNECTIONS).await;

        let mut extra = connect(&clipboards, "sb").await.unwrap();
        let mut answer = Vec::new();
        let closed = tokio::time::timeout(Duration::from_secs(2), extra.read_to_end(&mut answer))
            .await
            .unwrap();
        host.gate
            .as_ref()
            .unwrap()
            .add_permits(vsock::MAX_CONNECTIONS);
        let mut first = Vec::new();
        busy[0].read_to_end(&mut first).await.unwrap();

        assert!(closed.is_err() || answer.is_empty(), "{answer:?}");
        assert_eq!(first, b"ok 4\ntext");
    }

    /// A PNG signature and a few bytes that aren't valid UTF-8.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\xff";

    /// Sends each request on a connection of its own, one after the other, and returns the
    /// answers.
    async fn ask_each(clipboards: &Clipboards, requests: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut answers = Vec::new();

        for request in requests {
            answers.push(ask(clipboards, "sb", request).await);
        }

        answers
    }

    /// Returns a copy request for the content of the kind.
    fn copy_request(kind: &str, content: &[u8]) -> Vec<u8> {
        let mut request = format!("copy {kind} {}\n", content.len()).into_bytes();
        request.extend_from_slice(content);
        request
    }

    #[tokio::test]
    async fn copies_text_and_png_to_the_host_clipboard() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeClipboard::with(None, None));
        let clipboards = Clipboards::new(Some(dir.path().join("clipboard")), host.clone());
        clipboards.listen("sb", true).await.unwrap();

        let text = ask(&clipboards, "sb", &copy_request("text", "héllo".as_bytes())).await;
        let png = ask(&clipboards, "sb", &copy_request("png", PNG)).await;
        let empty = ask(&clipboards, "sb", &copy_request("text", b"")).await;

        assert_eq!([text, png, empty], [b"ok 0\n"; 3]);
        assert_eq!(
            host.written(),
            [
                (Kind::Text, "héllo".as_bytes().to_vec()),
                (Kind::Png, PNG.to_vec()),
                (Kind::Text, Vec::new()),
            ]
        );
    }

    #[tokio::test]
    async fn refuses_copies_over_the_limits_without_reading_them() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeClipboard::with(None, None));
        let clipboards = Clipboards::new(Some(dir.path().join("clipboard")), host.clone());
        clipboards.listen("sb", true).await.unwrap();
        let text = format!("copy text {}\n", MAX_TEXT + 1);
        let png = format!("copy png {}\n", MAX_PNG + 1);

        // Without the content: fbkd answers before reading it.
        let answers = [
            ask(&clipboards, "sb", text.as_bytes()).await,
            ask(&clipboards, "sb", png.as_bytes()).await,
        ];

        assert_eq!(answers, [b"too-large\n"; 2]);
        assert!(host.written().is_empty());
    }

    #[tokio::test]
    async fn copies_content_up_to_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeClipboard::with(None, None));
        let clipboards = Clipboards::new(Some(dir.path().join("clipboard")), host.clone());
        clipboards.listen("sb", true).await.unwrap();
        let text = vec![b'a'; MAX_TEXT];

        let answer = ask(&clipboards, "sb", &copy_request("text", &text)).await;

        assert_eq!(answer, b"ok 0\n");
        assert_eq!(host.written(), [(Kind::Text, text)]);
    }

    #[tokio::test]
    async fn refuses_content_that_isnt_valid_for_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeClipboard::with(None, None));
        let clipboards = Clipboards::new(Some(dir.path().join("clipboard")), host.clone());
        clipboards.listen("sb", true).await.unwrap();

        let answers = [
            ask(&clipboards, "sb", &copy_request("text", b"\xff\xfe")).await,
            ask(&clipboards, "sb", &copy_request("png", b"GIF89a")).await,
            ask(&clipboards, "sb", &copy_request("png", b"")).await,
        ];

        assert_eq!(answers, [b"bad-content\n"; 3]);
        assert!(host.written().is_empty());
    }

    #[tokio::test]
    async fn sets_nothing_when_the_content_ends_early() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeClipboard::with(None, None));
        let clipboards = Clipboards::new(Some(dir.path().join("clipboard")), host.clone());
        clipboards.listen("sb", true).await.unwrap();

        let answer = ask(&clipboards, "sb", b"copy text 10\nshort").await;

        assert!(answer.is_empty(), "{answer:?}");
        assert!(host.written().is_empty());
    }

    #[tokio::test]
    async fn answers_invalid_to_malformed_copy_requests() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeClipboard::with(None, None));
        let clipboards = Clipboards::new(Some(dir.path().join("clipboard")), host.clone());
        clipboards.listen("sb", true).await.unwrap();
        let requests: [&[u8]; 6] = [
            b"copy text\nhi",
            b"copy text +2\nhi",
            b"copy text 2 3\nhi",
            b"copy jpeg 2\nhi",
            b"copy  text 2\nhi",
            b"copy text 99999999999999999999999\n",
        ];

        let answers = ask_each(&clipboards, &requests).await;

        assert_eq!(answers, [b"invalid\n"; 6]);
        assert!(host.written().is_empty());
    }

    #[tokio::test]
    async fn denies_copies_from_a_sandbox_without_the_copy_label() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeClipboard::with(Some(b"text"), None));
        let clipboards = Clipboards::new(Some(dir.path().join("clipboard")), host.clone());
        clipboards.listen("sb", false).await.unwrap();

        let copy = ask(&clipboards, "sb", &copy_request("text", b"hi")).await;
        let paste = ask(&clipboards, "sb", b"text\n").await;

        assert_eq!(copy, b"denied\n");
        assert_eq!(paste, b"ok 4\ntext");
        assert!(host.written().is_empty());
    }

    #[tokio::test]
    async fn refuses_copies_beyond_the_rate_limit() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeClipboard::with(None, None));
        let clipboards = Clipboards::new(Some(dir.path().join("clipboard")), host.clone());
        clipboards.listen("sb", true).await.unwrap();

        let request = copy_request("text", b"hi");
        let answers = ask_each(&clipboards, &[&request[..]; MAX_COPIES + 1]).await;

        assert!(
            answers[..MAX_COPIES]
                .iter()
                .all(|answer| answer == b"ok 0\n")
        );
        assert_eq!(answers[MAX_COPIES], b"busy\n");
        assert_eq!(host.written().len(), MAX_COPIES);
    }

    #[tokio::test]
    async fn answers_unavailable_when_the_host_clipboard_cant_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeClipboard {
            available: false,
            ..FakeClipboard::with(None, None)
        };
        let clipboards = clipboards(dir.path(), host);
        clipboards.listen("sb", true).await.unwrap();

        let answer = ask(&clipboards, "sb", &copy_request("text", b"hi")).await;

        assert_eq!(answer, b"unavailable no-display\n");
    }

    #[tokio::test]
    async fn remove_stops_listening_and_deletes_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(b"text"), None));
        clipboards.listen("sb", true).await.unwrap();

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
    fn can_copy_needs_both_labels() {
        let labels = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>()
        };

        assert!(can_copy(&labels(&[
            (ENABLED_LABEL, "true"),
            (COPY_LABEL, "true")
        ])));
        assert!(!can_copy(&labels(&[(ENABLED_LABEL, "true")])));
        assert!(!can_copy(&labels(&[(COPY_LABEL, "true")])));
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
    fn detect_follows_the_platform_and_the_display_variables() {
        let set = |names: &'static [&'static str]| move |name: &str| names.contains(&name);

        assert_eq!(detect(true, set(&[])), Some(Desktop::MacOs));
        assert_eq!(
            detect(false, set(&["WAYLAND_DISPLAY", "DISPLAY"])),
            Some(Desktop::Wayland)
        );
        assert_eq!(detect(false, set(&["DISPLAY"])), Some(Desktop::X11));
        assert_eq!(detect(false, set(&[])), None);
    }

    #[test]
    fn a_tool_that_reports_nothing_copied_means_an_empty_clipboard() {
        let broken = failure("wl-paste", "Failed to connect to a Wayland server\n");

        assert_eq!(failure("wl-paste", "Nothing is copied\n"), HostError::Empty);
        assert_eq!(
            failure("xclip", "Error: target TARGETS not available\n"),
            HostError::Empty
        );
        assert_eq!(
            broken,
            HostError::Unavailable(
                Cause::Failed,
                "wl-paste failed: Failed to connect to a Wayland server".to_string()
            )
        );
    }

    // `HostTools` runs real processes, so `tests/host_tools.rs` tests it in a process of its
    // own: spawning processes here races with the tests in `forward`, which expect the port of
    // a closed listener to be free at once, while a forked child may still hold it.
    #[tokio::test]
    async fn read_at_most_stops_at_the_limit() {
        let output = [0; 100];

        let read = read_at_most(Some(&output[..]), 11).await.unwrap();
        let none = read_at_most(None::<&[u8]>, 11).await.unwrap();

        assert_eq!(read.len(), 11);
        assert!(none.is_empty());
    }
}
