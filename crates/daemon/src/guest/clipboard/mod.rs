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

use crate::guest::vsock::Listeners;
use async_trait::async_trait;
use microsandbox::sandbox::{SandboxBuilder, SandboxSpec};
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

mod host_tools;
mod protocol;

pub use host_tools::{Desktop, HostTools};
use protocol::{Channel, serve};

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
        let channel = Channel::new(sandbox, Arc::clone(&self.host), copy);

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

/// Returns the kinds that are present, text first.
fn kinds_where(present: impl Fn(Kind) -> bool) -> Vec<Kind> {
    [Kind::Text, Kind::Png]
        .into_iter()
        .filter(|kind| present(*kind))
        .collect()
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
    use super::protocol::MAX_REQUEST;
    use super::*;
    use crate::guest::vsock;
    use firebrick_utils::ipc;
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt;

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
        Clipboards::new(
            Some(vsock::test_endpoint_dir(dir, "clipboard")),
            Arc::new(host),
        )
    }

    async fn connect(clipboards: &Clipboards, sandbox: &str) -> io::Result<ipc::Stream> {
        ipc::connect(&clipboards.socket_path(sandbox).unwrap()).await
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
    async fn answers_invalid_to_unknown_requests() {
        let dir = tempfile::tempdir().unwrap();
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(b"text"), None));
        clipboards.listen("sb", true).await.unwrap();

        assert_eq!(ask(&clipboards, "sb", b"primary\n").await, b"invalid\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn answers_invalid_to_unfinished_requests() {
        let dir = tempfile::tempdir().unwrap();
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(b"text"), None));
        clipboards.listen("sb", true).await.unwrap();

        let answers = [
            ask(&clipboards, "sb", b"text").await,
            ask(&clipboards, "sb", b"").await,
        ];

        assert_eq!(answers, [b"invalid\n"; 2]);
    }

    /// A named pipe can't be half-closed, so on Windows a request that ends before its line
    /// isn't seen to end. fbkd closes it after [`REQUEST_TIMEOUT`] without an answer, and keeps
    /// answering other requests.
    #[cfg(windows)]
    #[tokio::test]
    async fn closes_unfinished_requests_after_the_request_timeout() {
        use super::protocol::REQUEST_TIMEOUT;
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let clipboards = clipboards(dir.path(), FakeClipboard::with(Some(b"text"), None));
        clipboards.listen("sb", true).await.unwrap();
        let started = Instant::now();

        let answer = ask(&clipboards, "sb", b"text").await;

        assert_eq!(answer, b"");
        assert!(
            started.elapsed() >= REQUEST_TIMEOUT,
            "{:?}",
            started.elapsed()
        );
        assert_eq!(ask(&clipboards, "sb", b"types\n").await, b"ok 5\ntext\n");
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
    async fn send_text_requests(clipboards: &Clipboards, count: usize) -> Vec<ipc::Stream> {
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
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(dir.path(), "clipboard")),
            host.clone(),
        );
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
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(dir.path(), "clipboard")),
            host.clone(),
        );
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
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(dir.path(), "clipboard")),
            host.clone(),
        );
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
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(dir.path(), "clipboard")),
            host.clone(),
        );
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
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(dir.path(), "clipboard")),
            host.clone(),
        );
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
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(dir.path(), "clipboard")),
            host.clone(),
        );
        clipboards.listen("sb", true).await.unwrap();

        let answer = ask(&clipboards, "sb", b"copy text 10\nshort").await;

        assert!(answer.is_empty(), "{answer:?}");
        assert!(host.written().is_empty());
    }

    #[tokio::test]
    async fn answers_invalid_to_malformed_copy_requests() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeClipboard::with(None, None));
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(dir.path(), "clipboard")),
            host.clone(),
        );
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
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(dir.path(), "clipboard")),
            host.clone(),
        );
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
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(dir.path(), "clipboard")),
            host.clone(),
        );
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

        assert!(!ipc::exists(&clipboards.socket_path("sb").unwrap()).await);
        assert!(connect(&clipboards, "sb").await.is_err());
    }

    #[tokio::test]
    async fn add_route_adds_the_marker_and_a_stream_route_to_the_socket() {
        let clipboards = Clipboards::new(
            Some(vsock::test_endpoint_dir(
                std::path::Path::new("/run/firebrick"),
                "clipboard",
            )),
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
}
