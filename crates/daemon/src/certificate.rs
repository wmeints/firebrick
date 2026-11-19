//! The localhost certificate authority that forwarded ports serve HTTPS with (ADR 0065).
//!
//! fbkd keeps a certificate authority in its data directory that may only sign certificates for
//! `localhost` and the loopback addresses. `fbk cert trust` adds it to the host's trust stores.
//! At every start fbkd signs a new server certificate with it, which it keeps in memory only, so
//! renewing that certificate never asks the user to trust anything again. The keys never leave
//! the host: a sandbox only ever sees the encrypted connections.

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, SanType,
};
use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

/// A server certificate for `localhost` and the key it was issued for.
pub struct HostCertificate {
    /// The server certificate.
    pub cert: CertificateDer<'static>,
    /// The private key of the server certificate.
    pub key: PrivateKeyDer<'static>,
}

/// What [`ensure_in`] did with the certificate authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaState {
    /// It used the stored certificate authority.
    Reused,
    /// It created one, because none was stored.
    Created,
    /// It replaced a stored one that couldn't be read or didn't match its key, so the host has
    /// to trust the new one again.
    Replaced,
}

/// Loads the certificate authority from fbkd's data directory, creating or replacing it when
/// needed, and issues a server certificate for `localhost` with it.
pub fn ensure() -> Result<HostCertificate> {
    let dir = firebrick_utils::localhost_ca_dir().context(firebrick_utils::NO_DATA_DIR)?;
    let (certificate, state) = ensure_in(&dir, OffsetDateTime::now_utc())?;

    match state {
        CaState::Reused => {}
        CaState::Created => tracing::info!(
            "created the localhost certificate authority in {}; run `fbk cert trust` to trust it",
            dir.display()
        ),
        CaState::Replaced => tracing::warn!(
            "replaced the localhost certificate authority in {}; run `fbk cert trust` again",
            dir.display()
        ),
    }

    Ok(certificate)
}

/// Loads the certificate authority from `dir`, creating it when it's missing and replacing it
/// when it can't be read or doesn't match its key, and issues a server certificate valid from
/// `now` with it. Fails only when `dir` can't be read or written.
pub fn ensure_in(dir: &Path, now: OffsetDateTime) -> Result<(HostCertificate, CaState)> {
    firebrick_utils::make_private_dir(dir)?;

    let state = match load_ca(dir)? {
        Some(ca) => match issue_valid(&ca, now) {
            Ok(certificate) => return Ok((certificate, CaState::Reused)),
            Err(err) => {
                tracing::debug!("can't use the localhost certificate authority: {err:#}");
                CaState::Replaced
            }
        },
        None if dir.join(CA_CERT_FILE).exists() || dir.join(CA_KEY_FILE).exists() => {
            CaState::Replaced
        }
        None => CaState::Created,
    };

    let ca = create_ca(dir, now)?;
    Ok((issue_valid(&ca, now)?, state))
}

/// How long a certificate authority is valid. It's never renewed: rustls and most browsers
/// don't check when a trusted certificate authority expires, and reading the date back would
/// take an X.509 parser.
const CA_VALIDITY: time::Duration = time::Duration::days(20 * 365);

/// How long a server certificate is valid: the most that macOS accepts.
const SERVER_VALIDITY: time::Duration = time::Duration::days(825);

/// How long before `now` certificates become valid, for clocks that run a little behind.
const BACKDATE: time::Duration = time::Duration::hours(1);

/// The file holding the certificate authority's certificate in PEM.
const CA_CERT_FILE: &str = "ca.crt";

/// The file holding the certificate authority's private key in PEM.
const CA_KEY_FILE: &str = "ca.key";

/// The names a server certificate is issued for.
const SERVER_DNS_NAMES: [&str; 2] = ["localhost", "*.dev.localhost"];

/// A certificate authority: its certificate and private key.
struct Ca {
    cert: CertificateDer<'static>,
    key: KeyPair,
}

/// Reads the certificate authority from `dir`, or returns `None` when a file is missing or
/// doesn't parse.
fn load_ca(dir: &Path) -> Result<Option<Ca>> {
    let (Some(cert), Some(key)) = (read(dir, CA_CERT_FILE)?, read(dir, CA_KEY_FILE)?) else {
        return Ok(None);
    };

    let cert = CertificateDer::from_pem_slice(cert.as_bytes());
    let key = KeyPair::from_pem(&key);

    Ok(cert.ok().zip(key.ok()).map(|(cert, key)| Ca { cert, key }))
}

/// Reads the file in `dir`, or returns `None` when it doesn't exist.
fn read(dir: &Path, name: &str) -> Result<Option<String>> {
    let path = dir.join(name);

    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Creates a certificate authority valid from `now` and stores it in `dir`.
fn create_ca(dir: &Path, now: OffsetDateTime) -> Result<Ca> {
    let key = KeyPair::generate()?;
    let mut params = ca_params();
    params.not_before = now - BACKDATE;
    params.not_after = now + CA_VALIDITY;
    let cert = params.self_signed(&key)?;

    // The key is written first: a certificate without its key is replaced at the next start.
    firebrick_utils::write_private(&dir.join(CA_KEY_FILE), &key.serialize_pem())?;
    firebrick_utils::write_private(&dir.join(CA_CERT_FILE), &cert.pem())?;

    Ok(Ca {
        cert: cert.der().clone(),
        key,
    })
}

/// Returns the parameters of the certificate authority, apart from its validity. Signing with
/// it only needs its name and key, so these must stay the same for the stored certificate
/// authorities to keep working.
fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, firebrick_utils::LOCALHOST_CA_NAME);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Firebrick");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![
            GeneralSubtree::DnsName("localhost".to_string()),
            GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix([127, 0, 0, 0], 8)),
            GeneralSubtree::IpAddress(CidrSubnet::from_addr_prefix(
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                128,
            )),
        ],
        excluded_subtrees: vec![],
    });

    params
}

/// Issues a server certificate valid from `now` with the certificate authority, and checks that
/// a client that trusts the certificate authority accepts it, which fails when the certificate
/// authority's certificate doesn't match its key.
fn issue_valid(ca: &Ca, now: OffsetDateTime) -> Result<HostCertificate> {
    let certificate = issue(ca, now)?;
    let verifier = verifier(ca)?;
    let at = u64::try_from(now.unix_timestamp())?;

    verifier.verify_server_cert(
        &certificate.cert,
        &[],
        &ServerName::try_from("localhost")?,
        &[],
        UnixTime::since_unix_epoch(Duration::from_secs(at)),
    )?;

    Ok(certificate)
}

/// Issues a server certificate for the [`SERVER_DNS_NAMES`] and the loopback addresses with the
/// certificate authority, valid from `now`.
fn issue(ca: &Ca, now: OffsetDateTime) -> Result<HostCertificate> {
    let key = KeyPair::generate()?;
    let names = SERVER_DNS_NAMES.map(String::from).to_vec();
    let mut params = CertificateParams::new(names)?;
    params.subject_alt_names.extend([
        SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        SanType::IpAddress(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    ]);
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    params.not_before = now - BACKDATE;
    params.not_after = now + SERVER_VALIDITY;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;

    let issuer = Issuer::new(ca_params(), &ca.key);
    let cert = params.signed_by(&key, &issuer)?;

    Ok(HostCertificate {
        cert: cert.der().clone(),
        key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    })
}

/// Returns a verifier that trusts only the certificate authority.
fn verifier(ca: &Ca) -> Result<Arc<WebPkiServerVerifier>> {
    let mut roots = RootCertStore::empty();
    roots.add(ca.cert.clone())?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

    Ok(WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a moment for tests, with whole seconds as certificates store them.
    fn moment() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    /// Reads the CA certificate that `fbk cert trust` adds to the trust stores.
    fn stored_ca(dir: &Path) -> CertificateDer<'static> {
        CertificateDer::from_pem_file(dir.join("ca.crt")).unwrap()
    }

    /// Verifies the server certificate for `name` at time `at`, as a client that trusts only the
    /// CA in `dir` does.
    fn verify(
        dir: &Path,
        certificate: &HostCertificate,
        name: &str,
        at: OffsetDateTime,
    ) -> Result<(), rustls::Error> {
        let mut roots = RootCertStore::empty();
        roots.add(stored_ca(dir)).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
            .build()
            .unwrap();
        let seconds = u64::try_from(at.unix_timestamp()).unwrap();

        verifier
            .verify_server_cert(
                &certificate.cert,
                &[],
                &ServerName::try_from(name.to_string()).unwrap(),
                &[],
                UnixTime::since_unix_epoch(Duration::from_secs(seconds)),
            )
            .map(|_| ())
    }

    #[test]
    fn creates_a_ca_that_trusts_the_certificate_for_localhost_names_only() {
        let dir = tempfile::tempdir().unwrap();

        let (certificate, state) = ensure_in(dir.path(), moment()).unwrap();

        assert_eq!(state, CaState::Created);
        for name in ["localhost", "app.dev.localhost", "127.0.0.1", "::1"] {
            verify(dir.path(), &certificate, name, moment())
                .unwrap_or_else(|err| panic!("{name} should be trusted: {err}"));
        }
        assert!(verify(dir.path(), &certificate, "example.com", moment()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_ca_key_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tls = dir.path().join("tls");

        ensure_in(&tls, moment()).unwrap();

        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&tls), 0o700);
        assert_eq!(mode(&tls.join("ca.key")), 0o600);
    }

    #[test]
    fn reuses_the_stored_ca_and_issues_a_new_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = ensure_in(dir.path(), moment()).unwrap();
        let ca = stored_ca(dir.path());

        let later = moment() + time::Duration::days(1000);
        let (second, state) = ensure_in(dir.path(), later).unwrap();

        assert_eq!(state, CaState::Reused);
        assert_eq!(stored_ca(dir.path()), ca);
        assert_ne!(second.cert, first.cert);
        verify(dir.path(), &second, "localhost", later).unwrap();
    }

    #[test]
    fn replaces_a_ca_that_cant_be_read() {
        let dir = tempfile::tempdir().unwrap();
        ensure_in(dir.path(), moment()).unwrap();
        std::fs::write(dir.path().join("ca.key"), "not a key").unwrap();

        let (certificate, state) = ensure_in(dir.path(), moment()).unwrap();

        assert_eq!(state, CaState::Replaced);
        verify(dir.path(), &certificate, "localhost", moment()).unwrap();
    }
}
