//! The endpoint the gRPC server listens on, a unix socket or a named pipe (see [`ipc`]), and
//! the connections it accepts there.

use super::{FirebrickServer, ServerError};
use crate::api::sandbox_management_service_server::SandboxManagementServiceServer;
use anyhow::Result;
use firebrick_utils::ipc;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Server;
use tonic::transport::server::Connected;

/// Serves the gRPC API on the endpoint, a unix socket or a named pipe (see [`ipc`]), with
/// `service` until `shutdown` completes, then removes the socket. Only clients that run as the
/// daemon's user or as root get through.
pub async fn serve(
    socket_path: &Path,
    service: FirebrickServer,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ServerError> {
    // Fails with `AddrInUse` for an endpoint that exists, also a stale socket file on Unix.
    let listener = ipc::Listener::bind(socket_path).map_err(bind_error)?;
    let (tx, rx) = mpsc::channel(16);
    let accepting = tokio::spawn(accept_loop(listener, tx));

    // Dropping the service when the server stops closes the forwards.
    let served = Server::builder()
        .trace_fn(|req| tracing::info_span!("grpc", path = %req.uri().path()))
        .add_service(SandboxManagementServiceServer::new(service))
        .serve_with_incoming_shutdown(ReceiverStream::new(rx), shutdown)
        .await;

    // Stop accepting, so the listener is closed, and on Windows its pipe gone, on return.
    accepting.abort();
    if let Some(err) = crate::guest::panicked(accepting.await) {
        tracing::error!("accepting connections failed: {err}");
    }
    served.map_err(ServerError::FailedToListen)?;
    ipc::remove(socket_path).await?;

    Ok(())
}

/// How long accepting waits after it failed for another reason than a client of another user,
/// so an error that persists, such as running out of file descriptors, doesn't spin.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Turns the error of binding the endpoint into the server's error, telling an endpoint that
/// another process serves from one that can't be created.
fn bind_error(err: io::Error) -> ServerError {
    match err.kind() {
        io::ErrorKind::AddrInUse => ServerError::SocketAlreadyInUse(),
        _ => ServerError::InvalidSocketPath(err),
    }
}

/// Sends each accepted connection to `tx` until it's closed. Connections from other users are
/// closed and logged.
async fn accept_loop(mut listener: ipc::Listener, tx: mpsc::Sender<io::Result<Connection>>) {
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = tx.closed() => return,
        };

        let stream = match accepted {
            Ok(stream) => stream,
            Err(err) => {
                not_accepted(&err).await;
                continue;
            }
        };

        if tx.send(Ok(Connection(stream))).await.is_err() {
            return;
        }
    }
}

/// Logs why a connection wasn't accepted. A client of another user is rejected, and its
/// connection closed when the error is dropped; after any other error accepting waits for
/// [`ACCEPT_RETRY_DELAY`].
async fn not_accepted(err: &io::Error) {
    if err.kind() == io::ErrorKind::PermissionDenied {
        tracing::warn!(error = %err, "rejected connection from another user");
        return;
    }

    tracing::warn!(error = %err, "failed to accept a connection");
    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
}

/// A client connection of the gRPC server.
struct Connection(ipc::Stream);

impl Connected for Connection {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
