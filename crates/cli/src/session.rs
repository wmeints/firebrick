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
use crate::manage::{ensure_running, resolve_spec, warn_if_outdated};

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

    while let Some(response) = responses.message().await? {
        match response.message {
            Some(attach_response::Message::Output(data)) => write_flushed(&mut stdout, &data)?,
            Some(attach_response::Message::ExitCode(code)) => return Ok(code),
            None => {}
        }
    }

    bail!("session ended without an exit code")
}

/// Writes data to the output and flushes it, so the terminal shows it right away. A byte the
/// output refuses as invalid UTF-8, as the Windows console does, is written as `U+FFFD`
/// instead of ending the session.
fn write_flushed(output: &mut impl Write, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        match output.write(data) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(written) => data = &data[written..],
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
                output.write_all(char::REPLACEMENT_CHARACTER.to_string().as_bytes())?;
                data = &data[1..];
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }

    output.flush()
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

    /// An output that, like the Windows console, writes the valid UTF-8 at the start of the data
    /// and refuses data that starts with a byte that isn't.
    #[derive(Default)]
    struct Console(Vec<u8>);

    impl Write for Console {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let valid = valid_utf8_prefix(data)?;
            self.0.extend_from_slice(&data[..valid]);
            Ok(valid)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Returns how many bytes at the start of the data the Windows console writes: the valid
    /// UTF-8, or the first byte of an incomplete character, which it buffers. Fails when the data
    /// starts with an invalid byte.
    fn valid_utf8_prefix(data: &[u8]) -> std::io::Result<usize> {
        match std::str::from_utf8(data) {
            Ok(text) => Ok(text.len()),
            Err(err) if err.valid_up_to() == 0 && err.error_len().is_some() => {
                Err(std::io::ErrorKind::InvalidData.into())
            }
            Err(err) => Ok(err.valid_up_to().max(data.len().min(1))),
        }
    }

    /// An output that accepts nothing.
    struct Closed;

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Ok(0)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_flushed_writes_utf8_unchanged() {
        let mut console = Console::default();

        write_flushed(&mut console, "héllo\x1b[0m".as_bytes()).unwrap();

        assert_eq!(console.0, "héllo\x1b[0m".as_bytes());
    }

    #[test]
    fn write_flushed_replaces_invalid_bytes_instead_of_failing() {
        let mut console = Console::default();

        write_flushed(&mut console, b"a\xffb\xfe").unwrap();

        assert_eq!(String::from_utf8(console.0).unwrap(), "a\u{fffd}b\u{fffd}");
    }

    #[test]
    fn write_flushed_fails_on_an_output_that_writes_nothing() {
        let err = write_flushed(&mut Closed, b"x").unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);
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
