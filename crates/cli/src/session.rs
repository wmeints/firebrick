use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Result, bail};
use crossterm::terminal;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tonic::Streaming;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

use crate::api::{
    AttachInput, AttachRequest, AttachResize, AttachResponse, AttachStart, attach_request,
    attach_response, sandbox_management_service_client::SandboxManagementServiceClient,
};
use crate::manage::{Selection, ensure_running, resolve_spec, warn_if_outdated};

const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// Keeps the terminal in raw mode for as long as it lives.
struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    /// Enables raw mode when stdin is a terminal.
    fn enable() -> Result<Self> {
        let enabled = std::io::stdin().is_terminal();

        if enabled {
            terminal::enable_raw_mode()?;
        }

        Ok(Self { enabled })
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
    warn_if_outdated(client, &name, Selection::WorkingDir).await;

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

/// Writes data to the output and flushes it, so the terminal shows it right away.
fn write_flushed(output: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    output.write_all(data)?;
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
async fn forward_resizes(tx: mpsc::Sender<AttachRequest>) {
    let Ok(mut window_changes) = signal(SignalKind::window_change()) else {
        return;
    };

    while window_changes.recv().await.is_some() {
        let request = AttachRequest {
            message: Some(attach_request::Message::Resize(window_size())),
        };

        if tx.send(request).await.is_err() {
            return;
        }
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
