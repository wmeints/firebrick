use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Result, bail};
use crossterm::terminal;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tonic::Streaming;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

use crate::api::{
    AttachInput, AttachRequest, AttachResize, AttachResponse, AttachStart, attach_request,
    attach_response, sandbox_management_service_client::SandboxManagementServiceClient,
};
use crate::sandbox::{ensure_running, resolve_spec, warn_if_outdated};

const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// How often `fbk run` checks the terminal size on Windows, which has no `SIGWINCH`.
#[cfg(windows)]
const RESIZE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Keeps the terminal in raw mode for as long as it lives.
struct RawModeGuard {
    enabled: bool,
    /// The console modes to restore once raw mode is off. Dropped after [`Drop::drop`] ran.
    #[cfg(windows)]
    _console: Option<vt::ConsoleModes>,
}

impl RawModeGuard {
    /// Enables raw mode when stdin is a terminal. On Windows it first has the console pass keys
    /// and render output as VT sequences, like a unix terminal, for the guest's PTY.
    fn enable() -> Result<Self> {
        let enabled = std::io::stdin().is_terminal();
        #[cfg(windows)]
        let console = enabled.then(vt::ConsoleModes::enable);

        if enabled {
            terminal::enable_raw_mode()?;
        }

        Ok(Self {
            enabled,
            #[cfg(windows)]
            _console: console,
        })
    }
}

impl Drop for RawModeGuard {
    /// Restores the terminal to normal mode.
    fn drop(&mut self) {
        if self.enabled {
            let _ = terminal::disable_raw_mode();
        }
    }
}

#[cfg(windows)]
mod vt;

/// Runs a command, given as the program followed by its arguments, in the sandbox attached to
/// the local terminal and returns its exit code. `assume_yes` approves changed mounts and
/// network settings of a new sandbox without asking.
pub async fn attach(
    working_dir: PathBuf,
    command: Vec<String>,
    assume_yes: bool,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<i32> {
    let mut words = command.into_iter();
    let Some(command) = words.next() else {
        bail!("no command to run");
    };
    let args = words.collect();
    let spec = resolve_spec(&working_dir, client).await?;
    let name = spec.name.clone();

    // Done before entering raw mode so progress messages render normally.
    ensure_running(spec, &working_dir, assume_yes, client).await?;
    warn_if_outdated(client, &name).await;

    let (tx, rx) = mpsc::channel(32);

    tx.send(AttachRequest {
        message: Some(attach_request::Message::Start(AttachStart {
            name,
            command,
            args,
            size: Some(window_size()),
        })),
    })
    .await?;

    let _guard = RawModeGuard::enable()?;

    let responses = client.attach(ReceiverStream::new(rx)).await?.into_inner();

    forward_stdin(tx.clone());
    let resize_task = tokio::spawn(forward_resizes(tx));

    let result = copy_output(responses).await;

    resize_task.abort();

    result
}

/// Writes session output to stdout until the session reports its exit code.
async fn copy_output(mut responses: Streaming<AttachResponse>) -> Result<i32> {
    let mut stdout = std::io::stdout();
    let mut text = ConsoleText::default();

    while let Some(response) = responses.message().await? {
        match response.message {
            Some(attach_response::Message::Output(data)) => {
                write_flushed(&mut stdout, &text.convert(&data))?;
            }
            Some(attach_response::Message::ExitCode(code)) => return Ok(code),
            None => {}
        }
    }

    bail!("session ended without an exit code")
}

/// Writes data to the output and flushes it, so the terminal shows it right away.
fn write_flushed(output: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    output.write_all(data)?;
    output.flush()
}

/// Turns the guest's terminal output into what the local terminal takes. The Windows console
/// refuses bytes that aren't UTF-8, also when stdout's buffer flushes them later, so there each
/// invalid sequence becomes `U+FFFD`, and a character split across two chunks is held back
/// until its rest arrives. Unix terminals take the bytes as they are.
#[derive(Default)]
struct ConsoleText {
    /// The start of a character whose rest is in the next chunk.
    pending: Vec<u8>,
}

impl ConsoleText {
    /// Returns the chunk as the local terminal takes it.
    fn convert(&mut self, data: &[u8]) -> Vec<u8> {
        if cfg!(windows) {
            self.sanitize(data)
        } else {
            data.to_vec()
        }
    }

    /// Returns the bytes held back and the chunk as valid UTF-8, with `U+FFFD` for every
    /// invalid sequence. Holds back an incomplete character at the end.
    fn sanitize(&mut self, data: &[u8]) -> Vec<u8> {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(data);
        let mut output = Vec::with_capacity(input.len());
        let mut chunks = input.utf8_chunks().peekable();

        while let Some(chunk) = chunks.next() {
            output.extend_from_slice(chunk.valid().as_bytes());
            let last = chunks.peek().is_none();
            self.hold_or_replace(chunk.invalid(), last, &mut output);
        }

        output
    }

    /// Holds back the invalid bytes when they're the `last` ones and may be the start of a
    /// character, and otherwise writes `U+FFFD` for them to the output.
    fn hold_or_replace(&mut self, invalid: &[u8], last: bool, output: &mut Vec<u8>) {
        if last && is_incomplete(invalid) {
            self.pending = invalid.to_vec();
        } else if !invalid.is_empty() {
            output.extend_from_slice("\u{fffd}".as_bytes());
        }
    }
}

/// Whether the bytes are the start of a UTF-8 character whose rest is missing.
fn is_incomplete(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && std::str::from_utf8(bytes)
            .err()
            .is_some_and(|err| err.error_len().is_none())
}

/// Sends stdin to the session from a plain thread. A tokio stdin reader would keep the runtime
/// from shutting down while it waits for input after the session has ended.
fn forward_stdin(tx: mpsc::Sender<AttachRequest>) {
    std::thread::spawn(move || copy_stdin(&tx));
}

/// Sends stdin to the session until stdin closes or the session goes away.
fn copy_stdin(tx: &mpsc::Sender<AttachRequest>) {
    let mut stdin = std::io::stdin();
    let mut buffer = [0u8; 1024];

    loop {
        let count = match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };

        let request = AttachRequest {
            message: Some(attach_request::Message::Input(AttachInput {
                data: buffer[..count].to_vec(),
            })),
        };

        if tx.blocking_send(request).is_err() {
            return;
        }
    }
}

/// Sends the new window size to the session whenever the terminal is resized.
#[cfg(unix)]
async fn forward_resizes(tx: mpsc::Sender<AttachRequest>) {
    let Ok(mut window_changes) = signal(SignalKind::window_change()) else {
        return;
    };

    while window_changes.recv().await.is_some() {
        if tx.send(resize_request(window_size())).await.is_err() {
            return;
        }
    }
}

/// Sends the new window size to the session whenever the terminal is resized, which Windows
/// only reports as a console input event, so the size is checked every
/// [`RESIZE_POLL_INTERVAL`] instead.
#[cfg(windows)]
async fn forward_resizes(tx: mpsc::Sender<AttachRequest>) {
    let mut interval = tokio::time::interval(RESIZE_POLL_INTERVAL);
    let mut last = window_size();

    loop {
        interval.tick().await;
        let size = window_size();
        if size == last {
            continue;
        }
        last = size;
        if tx.send(resize_request(size)).await.is_err() {
            return;
        }
    }
}

/// Returns the request that resizes the session's terminal.
fn resize_request(size: AttachResize) -> AttachRequest {
    AttachRequest {
        message: Some(attach_request::Message::Resize(size)),
    }
}

/// Returns the current terminal size, or a default size when it can't be determined.
fn window_size() -> AttachResize {
    let (width, height) = terminal::size().unwrap_or(DEFAULT_SIZE);

    AttachResize {
        width: width.into(),
        height: height.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::LineWriter;

    /// An output that, like the Windows console, refuses to write anything that isn't UTF-8.
    #[derive(Default)]
    struct Console(Vec<u8>);

    impl Write for Console {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            std::str::from_utf8(data).map_err(|_| std::io::ErrorKind::InvalidData)?;
            self.0.extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Writes the chunks to a console behind a line buffer, like stdout, after `convert`
    /// turned them into what the console takes, and returns what the console shows.
    fn show(
        chunks: &[&[u8]],
        convert: impl Fn(&mut ConsoleText, &[u8]) -> Vec<u8>,
    ) -> std::io::Result<String> {
        let mut stdout = LineWriter::new(Console::default());
        let mut text = ConsoleText::default();
        for chunk in chunks {
            write_flushed(&mut stdout, &convert(&mut text, chunk))?;
        }
        let console = stdout.into_inner().map_err(|err| err.into_error())?;
        Ok(String::from_utf8(console.0).unwrap())
    }

    #[test]
    fn a_buffered_console_refuses_invalid_bytes_when_it_flushes_them() {
        let raw = show(&[b"a\xff"], |_, chunk| chunk.to_vec());

        assert_eq!(raw.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn utf8_output_reaches_a_buffered_console_unchanged() {
        let shown = show(
            &["héllo\x1b[0m\n".as_bytes(), b"next"],
            ConsoleText::sanitize,
        );

        assert_eq!(shown.unwrap(), "héllo\x1b[0m\nnext");
    }

    #[test]
    fn invalid_bytes_reach_a_buffered_console_as_replacement_characters() {
        let shown = show(&[b"a\xffb\n", b"\xfe"], ConsoleText::sanitize);

        assert_eq!(shown.unwrap(), "a\u{fffd}b\n\u{fffd}");
    }

    #[test]
    fn a_character_split_across_chunks_is_held_back_until_it_is_complete() {
        let mut text = ConsoleText::default();

        assert_eq!(text.sanitize(b"h\xc3"), b"h");
        assert_eq!(text.sanitize(b"\xa9!"), "é!".as_bytes());
    }

    #[test]
    fn an_unfinished_character_followed_by_another_byte_is_replaced() {
        let mut text = ConsoleText::default();

        assert_eq!(text.sanitize(b"\xc3"), b"");
        assert_eq!(text.sanitize(b"x"), "\u{fffd}x".as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn unix_terminals_get_the_bytes_as_they_are() {
        let mut text = ConsoleText::default();

        assert_eq!(text.convert(b"a\xff"), b"a\xff");
    }

    #[test]
    fn resize_request_carries_the_size() {
        let size = AttachResize {
            width: 120,
            height: 40,
        };

        let request = resize_request(size);

        assert_eq!(request.message, Some(attach_request::Message::Resize(size)));
    }
}
