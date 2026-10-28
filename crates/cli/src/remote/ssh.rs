use std::io::{Read, Write};

use anyhow::Result;
use tokio::sync::mpsc;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

use crate::api::{
    SshTunnelRequest, sandbox_management_service_client::SandboxManagementServiceClient,
    ssh_tunnel_request,
};

/// Size of the chunks read from stdin. Matches the SSH channel packet size.
const CHUNK_SIZE: usize = 32 * 1024;

/// Tunnels an SSH connection between stdin/stdout and the sandbox with the given host name.
/// Used as `ProxyCommand` in the generated SSH config, so stdout carries only protocol bytes.
pub async fn proxy(
    hostname: String,
    client: &mut SandboxManagementServiceClient<Channel>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel(32);

    tx.send(SshTunnelRequest {
        message: Some(ssh_tunnel_request::Message::Hostname(hostname)),
    })
    .await?;

    let mut responses = client
        .ssh_tunnel(ReceiverStream::new(rx))
        .await?
        .into_inner();

    forward_stdin(tx);

    let mut stdout = std::io::stdout().lock();

    while let Some(response) = responses.message().await? {
        stdout.write_all(&response.data)?;
        stdout.flush()?;
    }

    Ok(())
}

/// Sends stdin to the tunnel from a plain thread. A tokio stdin reader would keep the runtime
/// from shutting down while it waits for input after the tunnel has closed.
fn forward_stdin(tx: mpsc::Sender<SshTunnelRequest>) {
    std::thread::spawn(move || copy_stdin(&tx));
}

/// Sends stdin to the tunnel until stdin closes or the tunnel goes away.
fn copy_stdin(tx: &mpsc::Sender<SshTunnelRequest>) {
    let mut stdin = std::io::stdin();
    let mut buffer = vec![0u8; CHUNK_SIZE];

    loop {
        let count = match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };

        let request = SshTunnelRequest {
            message: Some(ssh_tunnel_request::Message::Data(buffer[..count].to_vec())),
        };

        if tx.blocking_send(request).is_err() {
            return;
        }
    }
}
