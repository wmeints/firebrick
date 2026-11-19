//! TLS on forwarded ports (ADR 0065): a connection whose client starts a TLS handshake is
//! accepted with the host's localhost certificate and bridged to a TLS connection to the guest
//! port, so a host browser that trusts the localhost certificate authority opens an HTTPS dev
//! server in a sandbox without a warning. Other connections are relayed as they are.

use crate::certificate::HostCertificate;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::{LazyConfigAcceptor, TlsConnector};

use super::GuestIo;

/// The first byte of a TLS record that carries a handshake message, which is how every TLS
/// connection starts.
const TLS_HANDSHAKE: u8 = 0x16;

/// The server name fbkd connects to the guest with when the client didn't send one, such as
/// for `https://127.0.0.1`.
const DEFAULT_SERVER_NAME: &str = "localhost";

/// How many bytes of a guest that speaks first are read before relaying.
const GREETING_BUFFER: usize = 4096;

/// Accepts TLS connections with the host's localhost certificate and opens TLS connections to
/// the guest.
pub struct TlsBridge {
    server: ServerConfig,
    client: TlsConnector,
}

impl TlsBridge {
    /// Creates a bridge that accepts connections with the certificate. Fails when the
    /// certificate and its key don't match.
    pub fn new(certificate: HostCertificate) -> Result<Self, rustls::Error> {
        let HostCertificate { cert, key } = certificate;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)?;
        let algorithms = provider.signature_verification_algorithms;
        let client = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyGuestCertificate(algorithms)))
            .with_no_client_auth();

        Ok(Self {
            server,
            client: TlsConnector::from(Arc::new(client)),
        })
    }

    /// Relays the host connection to the guest connection until both sides are done. When the
    /// host's client opens with a TLS handshake, the bridge terminates it with the localhost
    /// certificate and relays the plain bytes over a TLS connection to the guest. A guest that
    /// speaks first, such as a database or SSH server, gets a plain relay. `target` names the
    /// guest port in the log.
    pub(super) async fn relay(
        &self,
        host: TcpStream,
        mut guest: Box<dyn GuestIo>,
        target: &str,
    ) -> io::Result<()> {
        match opening(&host, &mut guest).await? {
            Opening::ClientTls => self.bridge(host, guest, target).await,
            Opening::Plain(greeting) => relay_plain(host, guest, &greeting).await,
        }
    }

    /// Completes the client's handshake and a handshake with the guest that offers the same
    /// application protocols, then copies bytes between the two. A failed guest handshake is
    /// logged at `info`, because the browser only sees a closed connection.
    async fn bridge(
        &self,
        host: TcpStream,
        guest: Box<dyn GuestIo>,
        target: &str,
    ) -> io::Result<()> {
        let start = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), host).await?;
        let hello = start.client_hello();
        let protocols: Vec<Vec<u8>> = hello
            .alpn()
            .map(|offered| offered.map(<[u8]>::to_vec).collect())
            .unwrap_or_default();
        let name = server_name(hello.server_name())?;

        let connected = self.client.with_alpn(protocols).connect(name, guest).await;
        let mut guest = connected.inspect_err(|err| {
            tracing::info!(
                "a client of {target} speaks TLS, but the TLS handshake with the guest failed: \
                 {err}; serve HTTPS in the sandbox or connect with http://"
            );
        })?;
        let chosen = guest.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
        let mut host = start.into_stream(self.server_config(chosen)).await?;

        tokio::io::copy_bidirectional(&mut host, &mut guest).await?;
        Ok(())
    }

    /// Returns the server configuration that offers only the application protocol the guest
    /// chose, so the client speaks the same protocol as the guest.
    fn server_config(&self, protocol: Option<Vec<u8>>) -> Arc<ServerConfig> {
        let mut config = self.server.clone();
        config.alpn_protocols = protocol.into_iter().collect();

        Arc::new(config)
    }
}

/// Returns the server name to connect to the guest with: the one the client sent, or
/// [`DEFAULT_SERVER_NAME`].
fn server_name(sent: Option<&str>) -> io::Result<ServerName<'static>> {
    ServerName::try_from(sent.unwrap_or(DEFAULT_SERVER_NAME).to_string())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// How a forwarded connection starts.
enum Opening {
    /// The host's client sent a TLS handshake.
    ClientTls,
    /// The host's client sent something else or closed, or the guest spoke first and sent the
    /// greeting.
    Plain(Vec<u8>),
}

/// Waits until the host's client or the guest sends something, without taking the client's
/// bytes from the connection.
async fn opening(host: &TcpStream, guest: &mut Box<dyn GuestIo>) -> io::Result<Opening> {
    let mut first = [0; 1];
    let mut greeting = vec![0; GREETING_BUFFER];

    // Both futures are cancel safe: the one that loses the race has taken no bytes.
    tokio::select! {
        peeked = host.peek(&mut first) => Ok(match peeked? {
            1 if first[0] == TLS_HANDSHAKE => Opening::ClientTls,
            _ => Opening::Plain(vec![]),
        }),
        read = guest.read(&mut greeting) => {
            greeting.truncate(read?);
            Ok(Opening::Plain(greeting))
        }
    }
}

/// Sends the guest's greeting to the host, then copies bytes both ways until both sides are
/// done.
pub(super) async fn relay_plain(
    mut host: TcpStream,
    mut guest: Box<dyn GuestIo>,
    greeting: &[u8],
) -> io::Result<()> {
    host.write_all(greeting).await?;
    tokio::io::copy_bidirectional(&mut host, &mut guest).await?;

    Ok(())
}

/// Accepts every certificate of a guest's TLS server, but still checks that the server owns
/// its key. The guest is untrusted anyway, the connection to it runs through the sandbox's SSH
/// channel, and its dev servers serve certificates nobody issued, such as the ASP.NET Core
/// development certificate.
#[derive(Debug)]
struct AnyGuestCertificate(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyGuestCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}
