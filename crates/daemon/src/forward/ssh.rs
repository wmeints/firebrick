//! Reaches guest ports through `direct-tcpip` channels of the sandboxes' SSH servers.

use async_trait::async_trait;
use microsandbox::Sandbox;
use russh::client::{self, Handle};
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg, PublicKeyBase64};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::io::DuplexStream;
use tokio::sync::Mutex;

use super::{Connector, GuestIo};

/// Receive window of the SSH client. It is small, so the in-memory pipe below can always hold
/// a full window in each direction and neither side blocks on a full pipe.
const SSH_WINDOW_SIZE: u32 = 256 * 1024;

/// Buffer size of the in-memory pipe between the SSH client and server, per direction. It
/// holds the server's default window of 2 MiB plus the client's window and framing overhead.
const SSH_PIPE_SIZE: usize = 4 * 1024 * 1024;

/// Opens connections through `direct-tcpip` channels of the sandboxes' SSH servers, with one
/// SSH session per sandbox that is opened on first use and reopened when it has closed.
#[derive(Default)]
pub struct SshConnector {
    sessions: Mutex<HashMap<String, Arc<SessionSlot>>>,
}

/// The SSH session of one sandbox, if one was opened. Locked while the session opens, so
/// connections to the sandbox wait for it without blocking other sandboxes.
type SessionSlot = Mutex<Option<Arc<Handle<SshClient>>>>;

#[async_trait]
impl Connector for SshConnector {
    async fn connect(&self, sandbox: &str, guest_port: u16) -> io::Result<Box<dyn GuestIo>> {
        let session = self.session(sandbox).await?;
        let channel = session
            .channel_open_direct_tcpip("127.0.0.1", u32::from(guest_port), "127.0.0.1", 0)
            .await
            .map_err(io::Error::other)?;

        Ok(Box::new(channel.into_stream()))
    }

    async fn forget(&self, sandbox: &str) {
        self.sessions.lock().await.remove(sandbox);
    }
}

impl SshConnector {
    /// Returns the open SSH session to the sandbox, opening a new one when needed.
    async fn session(&self, sandbox: &str) -> io::Result<Arc<Handle<SshClient>>> {
        let slot = Arc::clone(
            self.sessions
                .lock()
                .await
                .entry(sandbox.to_string())
                .or_default(),
        );
        let mut slot = slot.lock().await;

        if let Some(session) = slot.as_ref().filter(|session| !session.is_closed()) {
            return Ok(Arc::clone(session));
        }

        let session = Arc::new(open_session(sandbox).await.map_err(|err| {
            tracing::warn!("couldn't open SSH session to sandbox {sandbox}: {err}");
            io::Error::other(err)
        })?);
        *slot = Some(Arc::clone(&session));

        Ok(session)
    }
}

/// Serves the sandbox's SSH server over an in-memory pipe and logs in to it. The keys are
/// generated for this session only, because the pipe never leaves the daemon.
async fn open_session(sandbox: &str) -> anyhow::Result<Handle<SshClient>> {
    let client_key = PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519)?;
    let client_end = serve_ssh(sandbox, &client_key).await?;

    let config = client::Config {
        window_size: SSH_WINDOW_SIZE,
        ..Default::default()
    };
    let mut session = client::connect_stream(Arc::new(config), client_end, SshClient).await?;
    let auth = session
        .authenticate_publickey(
            "root",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await?;

    anyhow::ensure!(auth.success(), "SSH public-key authentication failed");

    Ok(session)
}

/// Serves the sandbox's SSH server on one end of an in-memory pipe, accepting `client_key`, and
/// returns the other end.
async fn serve_ssh(sandbox: &str, client_key: &PrivateKey) -> anyhow::Result<DuplexStream> {
    let sb = Sandbox::get(sandbox).await?.connect().await?;
    let host_key = PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519)?;
    let authorized_key = client_key.public_key().public_key_base64();

    // Configured forwards stay open while the sandbox runs, however long they are idle.
    let server = sb
        .ssh()
        .server_with(|opts| {
            opts.host_key(host_key)
                .authorized_key(authorized_key)
                .disable_inactivity_timeout()
        })
        .await?;

    let (client_end, server_end) = tokio::io::duplex(SSH_PIPE_SIZE);
    let name = sandbox.to_string();
    tokio::spawn(async move {
        if let Err(err) = server.serve(server_end).await {
            tracing::debug!("SSH session for forwards of sandbox {name} ended: {err}");
        }
    });

    Ok(client_end)
}

/// The SSH client of the forwards.
pub struct SshClient;

impl client::Handler for SshClient {
    type Error = russh::Error;

    /// Accepts the server's key: the server runs in this process with a key generated for the
    /// session, and the pipe to it never leaves the daemon.
    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}
