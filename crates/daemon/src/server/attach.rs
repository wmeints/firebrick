//! `Attach` sessions: runs an interactive process with a terminal in a sandbox and streams
//! its input and output over the gRPC stream.

use crate::api::{AttachRequest, AttachResize, AttachResponse, attach_request, attach_response};
use microsandbox::Sandbox;
use microsandbox::sandbox::exec::{ExecControl, ExecEvent, ExecHandle, ExecSink};
use std::ops::ControlFlow;
use tokio::sync::mpsc;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Status, Streaming};

/// The terminal type of sessions. Without it, microsandbox passes on the daemon's own `TERM`,
/// which the guest may have no terminfo entry for, such as `xterm-ghostty`.
const SESSION_TERM: &str = "xterm-256color";

/// The command to run in a session and the size of its terminal as `(width, height)`.
#[derive(Debug, Clone)]
pub struct SessionCommand {
    /// The program to run.
    pub command: String,
    /// The arguments of the program.
    pub args: Vec<String>,
    /// The terminal size as PTY columns and rows.
    pub size: (u16, u16),
}

/// Starts the command in the connected sandbox and streams its output back, forwarding the
/// client messages from `inbound` to it until either side ends.
pub async fn attach(
    sb: Sandbox,
    inbound: Streaming<AttachRequest>,
    command: SessionCommand,
) -> Result<ReceiverStream<Result<AttachResponse, Status>>, Status> {
    let session = Session::open(&sb, command.command, command.args).await?;
    let (width, height) = command.size;
    session.resize(width, height).await?;

    let (tx, rx) = mpsc::channel(32);

    tokio::spawn(async move {
        run_session(inbound, session, tx).await;
        // The connected handle only owns the agent connection; dropping it leaves the VM running.
        drop(sb);
    });

    Ok(ReceiverStream::new(rx))
}

/// An interactive process in a sandbox, with its input and controls.
struct Session {
    /// Name of the sandbox the session runs in.
    sandbox: String,
    handle: ExecHandle,
    stdin: ExecSink,
    control: ExecControl,
}

impl Session {
    /// Starts the command with a terminal in the sandbox.
    async fn open(sb: &Sandbox, command: String, args: Vec<String>) -> Result<Self, Status> {
        let mut handle = sb
            .exec_stream_with(command, |e| {
                e.args(args)
                    .env("TERM", SESSION_TERM)
                    .stdin_pipe()
                    .tty(true)
            })
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "failed to start session in sandbox {}", sb.name());
                Status::internal("failed to start session")
            })?;

        let stdin = handle
            .take_stdin()
            .ok_or_else(|| Status::internal("session has no stdin"))?;
        let control = handle.control();

        Ok(Self {
            sandbox: sb.name().to_string(),
            handle,
            stdin,
            control,
        })
    }

    /// Sets the terminal size of the session, ending the session when that fails.
    async fn resize(&self, width: u16, height: u16) -> Result<(), Status> {
        if let Err(err) = self.control.resize(height, width).await {
            tracing::error!(error = ?err, "failed to resize session in sandbox {}", self.sandbox);
            end_session(&self.control).await;
            return Err(Status::internal("failed to resize session"));
        }

        Ok(())
    }
}

/// Forwards client input to the session and session output to the client until either side ends.
async fn run_session(
    mut inbound: Streaming<AttachRequest>,
    mut session: Session,
    tx: mpsc::Sender<Result<AttachResponse, Status>>,
) {
    loop {
        let flow = tokio::select! {
            message = inbound.message() => {
                handle_client_message(message, &session.stdin, &session.control).await
            }
            event = session.handle.recv() => forward_event(event, &tx, &session.control).await,
            _ = tx.closed() => disconnect(&session.control).await,
        };

        if flow.is_break() {
            return;
        }
    }
}

/// Applies a message from the client to the session, ending the session when the client is gone.
async fn handle_client_message(
    message: Result<Option<AttachRequest>, Status>,
    stdin: &ExecSink,
    control: &ExecControl,
) -> ControlFlow<()> {
    match message {
        Ok(Some(AttachRequest {
            message: Some(message),
        })) => apply_client_message(message, stdin, control).await,
        Ok(Some(AttachRequest { message: None })) => {}
        Ok(None) | Err(_) => return disconnect(control).await,
    }

    ControlFlow::Continue(())
}

/// Writes client input to the session or resizes it.
async fn apply_client_message(
    message: attach_request::Message,
    stdin: &ExecSink,
    control: &ExecControl,
) {
    match message {
        attach_request::Message::Input(input) => write_input(stdin, input.data).await,
        attach_request::Message::Resize(size) => resize_session(control, size).await,
        attach_request::Message::Start(_) => {
            tracing::warn!("ignoring start message on running session");
        }
    }
}

/// Writes client input to the session, logging a warning when that fails.
async fn write_input(stdin: &ExecSink, data: Vec<u8>) {
    if let Err(err) = stdin.write(data).await {
        tracing::warn!("failed to write session input: {err}");
    }
}

/// Resizes the session terminal, logging a warning when that fails.
async fn resize_session(control: &ExecControl, size: AttachResize) {
    let Ok((width, height)) = window_size(Some(size)) else {
        tracing::warn!("ignoring invalid resize request");
        return;
    };

    if let Err(err) = control.resize(height, width).await {
        tracing::warn!("failed to resize session: {err}");
    }
}

/// Sends session output and its exit code to the client, stopping when the session ends.
async fn forward_event(
    event: Option<ExecEvent>,
    tx: &mpsc::Sender<Result<AttachResponse, Status>>,
    control: &ExecControl,
) -> ControlFlow<()> {
    match event {
        Some(ExecEvent::Stdout(data) | ExecEvent::Stderr(data)) => {
            forward_output(data.to_vec(), tx, control).await
        }
        Some(ExecEvent::Exited { code }) => {
            let response = AttachResponse {
                message: Some(attach_response::Message::ExitCode(code)),
            };
            let _ = tx.send(Ok(response)).await;
            ControlFlow::Break(())
        }
        Some(ExecEvent::Failed(failed)) => {
            tracing::error!(error = ?failed, "session failed to start");
            let _ = tx
                .send(Err(Status::internal("session failed to start")))
                .await;
            ControlFlow::Break(())
        }
        Some(_) => ControlFlow::Continue(()),
        None => ControlFlow::Break(()),
    }
}

/// Sends session output to the client, ending the session when the client is gone.
async fn forward_output(
    data: Vec<u8>,
    tx: &mpsc::Sender<Result<AttachResponse, Status>>,
    control: &ExecControl,
) -> ControlFlow<()> {
    let response = AttachResponse {
        message: Some(attach_response::Message::Output(data)),
    };

    if tx.send(Ok(response)).await.is_err() {
        end_session(control).await;
        return ControlFlow::Break(());
    }

    ControlFlow::Continue(())
}

/// Ends the session because the client disconnected.
async fn disconnect(control: &ExecControl) -> ControlFlow<()> {
    tracing::info!("client disconnected, ending session");
    end_session(control).await;
    ControlFlow::Break(())
}

/// Kills the session, logging a warning when that fails.
async fn end_session(control: &ExecControl) {
    if let Err(err) = control.kill().await {
        tracing::warn!("failed to end session: {err}");
    }
}

/// Converts a requested window size to `(width, height)` as PTY columns and rows.
pub(crate) fn window_size(size: Option<AttachResize>) -> Result<(u16, u16), Status> {
    let size = size.ok_or_else(|| Status::invalid_argument("window size is required"))?;

    let width = u16::try_from(size.width)
        .map_err(|_| Status::invalid_argument("window width is too large"))?;
    let height = u16::try_from(size.height)
        .map_err(|_| Status::invalid_argument("window height is too large"))?;

    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_size_converts_to_width_and_height() {
        let size = AttachResize {
            width: 120,
            height: 40,
        };

        assert_eq!(window_size(Some(size)).unwrap(), (120, 40));
    }

    #[test]
    fn window_size_rejects_missing_and_oversized_values() {
        let oversized = AttachResize {
            width: u32::from(u16::MAX) + 1,
            height: 40,
        };

        assert_eq!(
            window_size(None).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            window_size(Some(oversized)).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }
}
