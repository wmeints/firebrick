//! The protocol between the clipboard stand-ins in the guest and fbkd: parses a connection's
//! request, reads or writes the host clipboard through the [`HostClipboard`] and answers.

use crate::guest::rate_limit::RateLimit;
use crate::guest::vsock::{self, Connections, Refused};
use firebrick_utils::ipc;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use super::{COPY_WINDOW, Cause, HostClipboard, HostError, Kind, MAX_COPIES, read_at_most};

/// The longest request line fbkd reads, in bytes, without its newline.
pub(super) const MAX_REQUEST: usize = 32;

/// How long a stand-in gets to send its request, and then the content of a copy.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How often fbkd logs a warning about one sandbox's clipboard at most.
const WARNING_INTERVAL: Duration = Duration::from_secs(60);

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
pub(super) struct Channel {
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
    /// Creates the channel of the sandbox that reads the clipboard from `host`, and lets the
    /// sandbox write it when `copy` is set.
    pub(super) fn new(sandbox: &str, host: Arc<dyn HostClipboard>, copy: bool) -> Self {
        Self {
            sandbox: sandbox.to_string(),
            host,
            copy,
            copies: Mutex::new(RateLimit::new(MAX_COPIES, COPY_WINDOW)),
            last_warning: Mutex::new(None),
        }
    }

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
async fn serve_connection(stream: ipc::Stream, channel: Arc<Channel>) {
    let (read, mut write) = tokio::io::split(stream);
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
    accepted: io::Result<ipc::Stream>,
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
pub(super) async fn serve(
    mut listener: ipc::Listener,
    channel: Arc<Channel>,
    shutdown: oneshot::Receiver<()>,
) {
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
