//! `SshTunnel` connections: serves microsandbox's SSH server of a sandbox over the gRPC
//! stream through an in-memory pipe.

use crate::api::{SshTunnelRequest, SshTunnelResponse, ssh_tunnel_request};
use crate::ssh;
use microsandbox::Sandbox;
use microsandbox::sandbox::ssh::SshServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Status, Streaming};

/// Buffer size of the in-memory pipe between a tunnel and its SSH server, per direction.
const TUNNEL_BUFFER_SIZE: usize = 256 * 1024;

/// Largest chunk of SSH protocol bytes sent to the client in a single message.
const TUNNEL_CHUNK_SIZE: usize = 32 * 1024;

/// Serves an SSH connection to the connected sandbox over the stream: the client bytes from
/// `inbound` go to the sandbox's SSH server, and the server bytes come back on the returned
/// stream until the session closes.
pub async fn open(
    sb: Sandbox,
    inbound: Streaming<SshTunnelRequest>,
    hostname: String,
) -> Result<ReceiverStream<Result<SshTunnelResponse, Status>>, Status> {
    let server = ssh_server(&sb).await?;
    let (tx, rx) = mpsc::channel(32);

    tokio::spawn(async move {
        serve_tunnel(server, inbound, tx, &hostname).await;
        // The connected handle only owns the agent connection; dropping it leaves the VM running.
        drop(sb);
    });

    Ok(ReceiverStream::new(rx))
}

/// Prepares the SSH server of a sandbox for a tunnel.
async fn ssh_server(sb: &Sandbox) -> Result<SshServer, Status> {
    let (host_key, client_key) = ssh::host_key_path()
        .and_then(|host_key| Ok((host_key, ssh::client_public_key_path()?)))
        .map_err(|err| {
            tracing::error!(error = ?err, "failed to locate the SSH keys");
            Status::internal("failed to locate the SSH keys")
        })?;
    // The proxy process owns the connection, so it decides when the session ends.
    sb.ssh()
        .server_with(|opts| {
            opts.host_key_path(host_key)
                .authorized_keys_path(client_key)
                .disable_inactivity_timeout()
        })
        .await
        .map_err(|err| {
            tracing::error!(error = ?err, "failed to prepare SSH server of sandbox {}", sb.name());
            Status::internal("failed to prepare SSH server")
        })
}

/// Serves an SSH session through the tunnel until it closes.
async fn serve_tunnel(
    server: SshServer,
    inbound: Streaming<SshTunnelRequest>,
    tx: mpsc::Sender<Result<SshTunnelResponse, Status>>,
    hostname: &str,
) {
    let (client_end, server_end) = tokio::io::duplex(TUNNEL_BUFFER_SIZE);
    let serve = tokio::spawn(async move { server.serve(server_end).await });

    run_tunnel(inbound, client_end, tx).await;

    match serve.await {
        Ok(Err(err)) => tracing::info!("SSH session to {hostname} ended: {err}"),
        Err(err) => tracing::warn!("SSH server for {hostname} failed: {err}"),
        Ok(Ok(())) => {}
    }
}

/// Copies client bytes to the SSH server and server bytes to the client until the server
/// closes its end or the client goes away.
async fn run_tunnel(
    inbound: Streaming<SshTunnelRequest>,
    stream: DuplexStream,
    tx: mpsc::Sender<Result<SshTunnelResponse, Status>>,
) {
    let (reader, writer) = tokio::io::split(stream);

    // The server closes its end in response to the end of input, which ends downstream.
    let upstream = async {
        copy_upstream(inbound, writer).await;
        std::future::pending::<()>().await;
    };

    // Both directions run concurrently so a full pipe in one direction can't stall the other.
    tokio::select! {
        _ = upstream => {}
        _ = copy_downstream(reader, &tx) => {}
    }
}

/// Copies client bytes to the SSH server, then signals the end of input.
async fn copy_upstream(
    mut inbound: Streaming<SshTunnelRequest>,
    mut writer: WriteHalf<DuplexStream>,
) {
    while let Some(data) = next_tunnel_data(&mut inbound).await {
        if writer.write_all(&data).await.is_err() {
            break;
        }
    }

    let _ = writer.shutdown().await;
}

/// Returns the next bytes the client sends, or `None` when the client goes away.
async fn next_tunnel_data(inbound: &mut Streaming<SshTunnelRequest>) -> Option<Vec<u8>> {
    loop {
        match inbound.message().await.ok()??.message {
            Some(ssh_tunnel_request::Message::Data(data)) => return Some(data),
            Some(ssh_tunnel_request::Message::Hostname(_)) => {
                tracing::warn!("ignoring hostname message on open tunnel");
            }
            None => {}
        }
    }
}

/// Copies SSH server bytes to the client until the server closes its end or the client goes
/// away.
async fn copy_downstream(
    mut reader: ReadHalf<DuplexStream>,
    tx: &mpsc::Sender<Result<SshTunnelResponse, Status>>,
) {
    let mut buffer = vec![0; TUNNEL_CHUNK_SIZE];

    while let Ok(count @ 1..) = reader.read(&mut buffer).await {
        let response = SshTunnelResponse {
            data: buffer[..count].to_vec(),
        };

        if tx.send(Ok(response)).await.is_err() {
            return;
        }
    }
}
