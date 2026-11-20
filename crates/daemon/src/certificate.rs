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
    pub(crate) cert: CertificateDer<'static>,
    /// The private key of the server certificate.
    pub(crate) key: PrivateKeyDer<'static>,
}

/// What [`ensure_in`] did with the certificate authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaState {
    /// It used the stored certificate authority.
    Reused,
    /// It created one, because none was stored.
    Created,
    /// It replaced a stored one for the reason given, because a file didn't parse or the
    /// certificate didn't match its key, so the host has to trust the new one again.
    Replaced(String),
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
        CaState::Replaced(reason) => tracing::warn!(
            "replaced the localhost certificate authority in {} because {reason}; run `fbk cert \
             trust` again",
            dir.display()
        ),
    }

    Ok(certificate)
}

/// Loads the certificate authority from `dir`, creating it when it's missing and replacing it
/// when a file doesn't parse or a client that trusts it rejects the certificate it issues, and
/// issues a server certificate valid from `now` with it. Any other error, such as a directory
/// that can't be written, fails instead of replacing a certificate authority the host trusts.
pub fn ensure_in(dir: &Path, now: OffsetDateTime) -> Result<(HostCertificate, CaState)> {
    firebrick_utils::make_private_dir(dir)?;
    let at = unix_time(now)?;

    let state = match load_ca(dir)? {
        Loaded::Usable(ca) => {
            let certificate = issue(&ca, now)?;
            match check(&ca, &certificate, at) {
                Ok(()) => return Ok((certificate, CaState::Reused)),
                Err(err) => CaState::Replaced(format!("it rejects its own certificates: {err:#}")),
            }
        }
        Loaded::Unusable(reason) => CaState::Replaced(reason),
        Loaded::Missing => CaState::Created,
    };

    let ca = create_ca(dir, now)?;
    let certificate = issue(&ca, now)?;
    check(&ca, &certificate, at).context("the new localhost certificate authority is unusable")?;

    Ok((certificate, state))
}

/// Returns `now` as the time rustls checks certificates at. Fails for a time before 1970.
fn unix_time(now: OffsetDateTime) -> Result<UnixTime> {
    let seconds = u64::try_from(now.unix_timestamp())
        .with_context(|| format!("the clock is set to {now}, before 1970"))?;

    Ok(UnixTime::since_unix_epoch(Duration::from_secs(seconds)))
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
const CA_CERT_FILE: &str = firebrick_utils::LOCALHOST_CA_CERT_FILE;

/// The file holding the certificate authority's private key in PEM.
const CA_KEY_FILE: &str = "ca.key";

/// The names a server certificate is issued for.
const SERVER_DNS_NAMES: [&str; 2] = ["localhost", "*.dev.localhost"];

/// A certificate authority: its certificate and private key.
struct Ca {
    cert: CertificateDer<'static>,
    key: KeyPair,
}

/// The certificate authority found in a directory.
enum Loaded {
    /// Neither file exists.
    Missing,
    /// A file is missing or doesn't parse, for the reason given.
    Unusable(String),
    /// Both files parse.
    Usable(Ca),
}

/// Reads the certificate authority from `dir`. Fails only when a file can't be read.
fn load_ca(dir: &Path) -> Result<Loaded> {
    let (cert, key) = match (read(dir, CA_CERT_FILE)?, read(dir, CA_KEY_FILE)?) {
        (None, None) => return Ok(Loaded::Missing),
        (Some(cert), Some(key)) => (cert, key),
        (None, Some(_)) => return Ok(Loaded::Unusable(format!("{CA_CERT_FILE} is missing"))),
        (Some(_), None) => return Ok(Loaded::Unusable(format!("{CA_KEY_FILE} is missing"))),
    };

    let cert = match CertificateDer::from_pem_slice(cert.as_bytes()) {
        Ok(cert) => cert,
        Err(err) => {
            return Ok(Loaded::Unusable(format!(
                "{CA_CERT_FILE} doesn't parse: {err}"
            )));
        }
    };

    Ok(match KeyPair::from_pem(&key) {
        Ok(key) => Loaded::Usable(Ca { cert, key }),
        Err(err) => Loaded::Unusable(format!("{CA_KEY_FILE} doesn't parse: {err}")),
    })
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
    let key = KeyPair::generate().context("failed to generate the localhost CA key")?;
    let mut params = ca_params();
    params.not_before = now - BACKDATE;
    params.not_after = now + CA_VALIDITY;
    let cert = params
        .self_signed(&key)
        .context("failed to create the localhost CA certificate")?;

    // The key is written first: a certificate without its key is replaced at the next start.
    write(&dir.join(CA_KEY_FILE), &key.serialize_pem())?;
    write(&dir.join(CA_CERT_FILE), &cert.pem())?;

    Ok(Ca {
        cert: cert.into(),
        key,
    })
}

/// Writes a file that only the user can read, naming it when that fails.
fn write(path: &Path, content: &str) -> Result<()> {
    firebrick_utils::write_private(path, content)
        .with_context(|| format!("failed to write {}", path.display()))
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

/// Checks that a client that trusts the certificate authority accepts the certificate at time
/// `at`, which fails when the certificate authority's certificate doesn't match its key.
fn check(ca: &Ca, certificate: &HostCertificate, at: UnixTime) -> Result<()> {
    let mut roots = RootCertStore::empty();
    roots.add(ca.cert.clone())?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier =
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build()?;
    let name = ServerName::try_from(SERVER_DNS_NAMES[0])?;

    verifier.verify_server_cert(&certificate.cert, &[], &name, &[], at)?;
    Ok(())
}

/// Issues a server certificate for the [`SERVER_DNS_NAMES`] and the loopback addresses with the
/// certificate authority, valid from `now`.
fn issue(ca: &Ca, now: OffsetDateTime) -> Result<HostCertificate> {
    let loopback = [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ];

    issue_for(ca, now, &SERVER_DNS_NAMES, &loopback)
        .context("failed to issue the localhost server certificate")
}

/// Issues a server certificate for the DNS names and IP addresses with the certificate
/// authority, valid from `now`.
fn issue_for(
    ca: &Ca,
    now: OffsetDateTime,
    dns_names: &[&str],
    addresses: &[IpAddr],
) -> Result<HostCertificate> {
    let key = KeyPair::generate()?;
    let names: Vec<String> = dns_names.iter().map(ToString::to_string).collect();
    let mut params = CertificateParams::new(names)?;
    params
        .subject_alt_names
        .extend(addresses.iter().copied().map(SanType::IpAddress));
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
        cert: cert.into(),
        key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    })
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

    /// Issues a certificate for the names with the CA stored in `dir`, as a stolen CA key
    /// would.
    fn issue_with_stored_ca(
        dir: &Path,
        dns_names: &[&str],
        addresses: &[IpAddr],
    ) -> HostCertificate {
        let Loaded::Usable(ca) = load_ca(dir).unwrap() else {
            panic!("the stored CA should load");
        };

        issue_for(&ca, moment(), dns_names, addresses).unwrap()
    }

    #[test]
    fn creates_a_ca_whose_certificate_is_trusted_for_localhost_names() {
        let dir = tempfile::tempdir().unwrap();

        let (certificate, state) = ensure_in(dir.path(), moment()).unwrap();

        assert_eq!(state, CaState::Created);
        for name in ["localhost", "app.dev.localhost", "127.0.0.1", "::1"] {
            verify(dir.path(), &certificate, name, moment())
                .unwrap_or_else(|err| panic!("{name} should be trusted: {err}"));
        }
    }

    #[test]
    fn ca_can_only_vouch_for_localhost_names_and_loopback_addresses() {
        let dir = tempfile::tempdir().unwrap();
        ensure_in(dir.path(), moment()).unwrap();
        let loopback = issue_with_stored_ca(
            dir.path(),
            &["api.localhost"],
            &["127.0.0.2".parse().unwrap()],
        );
        let foreign =
            issue_with_stored_ca(dir.path(), &["example.com"], &["10.0.0.1".parse().unwrap()]);

        verify(dir.path(), &loopback, "api.localhost", moment()).unwrap();
        verify(dir.path(), &loopback, "127.0.0.2", moment()).unwrap();
        for name in ["example.com", "10.0.0.1"] {
            let err = verify(dir.path(), &foreign, name, moment()).unwrap_err();
            assert!(
                format!("{err:?}").contains("NameConstraintViolation"),
                "{name} should violate the name constraint: {err:?}"
            );
        }
    }

    #[test]
    fn server_certificate_is_valid_for_825_days_from_an_hour_ago() {
        let dir = tempfile::tempdir().unwrap();
        let (certificate, _) = ensure_in(dir.path(), moment()).unwrap();
        let at = |offset| moment() + offset;

        verify(
            dir.path(),
            &certificate,
            "localhost",
            at(time::Duration::minutes(-59)),
        )
        .unwrap();
        verify(
            dir.path(),
            &certificate,
            "localhost",
            at(time::Duration::days(825)),
        )
        .unwrap();
        assert!(
            verify(
                dir.path(),
                &certificate,
                "localhost",
                at(time::Duration::minutes(-61))
            )
            .is_err()
        );
        assert!(
            verify(
                dir.path(),
                &certificate,
                "localhost",
                at(time::Duration::days(825) + time::Duration::seconds(1))
            )
            .is_err()
        );
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
    fn replaces_a_ca_that_cant_be_parsed_is_incomplete_or_doesnt_match_its_key() {
        let other_key = KeyPair::generate().unwrap().serialize_pem();
        let damages: [(&str, Option<&str>, &str); 3] = [
            ("ca.key", Some("not a key"), "ca.key doesn't parse"),
            ("ca.key", None, "ca.key is missing"),
            (
                "ca.key",
                Some(&other_key),
                "it rejects its own certificates",
            ),
        ];

        for (file, content, reason) in damages {
            let dir = tempfile::tempdir().unwrap();
            ensure_in(dir.path(), moment()).unwrap();
            let ca = stored_ca(dir.path());
            match content {
                Some(content) => std::fs::write(dir.path().join(file), content).unwrap(),
                None => std::fs::remove_file(dir.path().join(file)).unwrap(),
            }

            let (certificate, state) = ensure_in(dir.path(), moment()).unwrap();

            assert!(
                matches!(&state, CaState::Replaced(given) if given.starts_with(reason)),
                "the CA should be replaced because {reason}, not {state:?}"
            );
            assert_ne!(stored_ca(dir.path()), ca, "{reason}");
            verify(dir.path(), &certificate, "localhost", moment()).unwrap();
        }
    }

    #[test]
    fn clock_before_1970_fails_without_replacing_the_ca() {
        let dir = tempfile::tempdir().unwrap();
        ensure_in(dir.path(), moment()).unwrap();
        let ca = stored_ca(dir.path());

        let before_1970 = OffsetDateTime::from_unix_timestamp(-1).unwrap();
        let err = ensure_in(dir.path(), before_1970).err().unwrap();

        assert!(err.to_string().contains("before 1970"), "{err:#}");
        assert_eq!(stored_ca(dir.path()), ca);
    }
}
