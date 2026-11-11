//! The public root certificates that TLS interception trusts for upstream servers on Windows
//! hosts. microsandbox verifies upstream servers against the roots in the host's store, and
//! Windows installs most roots only when CryptoAPI first builds a chain to them, so fbkd
//! writes the Mozilla roots to a PEM file that the proxy trusts as well (ADR 0061).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::io;
use std::path::Path;
use tokio::sync::Mutex;

/// Where the bundle lives in the microsandbox home, next to its TLS interception CA.
pub const BUNDLE_PATH: &str = "tls/upstream-roots.pem";

/// Length of the base64 lines in a PEM block.
const LINE_LENGTH: usize = 64;

/// Returns the DER certificates as PEM, one `CERTIFICATE` block each.
pub fn pem(certs: &[impl AsRef<[u8]>]) -> String {
    certs.iter().map(|cert| pem_block(cert.as_ref())).collect()
}

/// Returns one certificate as a PEM block with lines of [`LINE_LENGTH`] characters.
fn pem_block(der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let lines: Vec<&str> = encoded
        .as_bytes()
        .chunks(LINE_LENGTH)
        // base64 is ASCII, so every chunk is valid UTF-8.
        .filter_map(|line| std::str::from_utf8(line).ok())
        .collect();

    format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        lines.join("\n")
    )
}

/// Writes the contents to the file unless it already holds exactly them, creating its
/// directory. The file is replaced atomically, so a sandbox that starts meanwhile reads either
/// the old or the new bundle. Sandboxes created at the same time write one after the other,
/// because the writes share a temporary file.
pub async fn write(path: &Path, contents: String) -> io::Result<()> {
    static WRITING: Mutex<()> = Mutex::const_new(());
    let _writing = WRITING.lock().await;

    if tokio::fs::read(path).await.ok().as_deref() == Some(contents.as_bytes()) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || firebrick_utils::write_atomic(&path, &contents, 0o600))
        .await
        .map_err(io::Error::other)?
}

/// Writes the Mozilla roots to the file, as [`write`] does.
pub async fn ensure(path: &Path) -> io::Result<()> {
    write(path, pem(webpki_root_certs::TLS_SERVER_ROOT_CERTS)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_writes_one_block_per_certificate() {
        assert_eq!(
            pem(&[b"hello".as_slice(), b"world!".as_slice()]),
            "-----BEGIN CERTIFICATE-----\naGVsbG8=\n-----END CERTIFICATE-----\n\
             -----BEGIN CERTIFICATE-----\nd29ybGQh\n-----END CERTIFICATE-----\n"
        );
    }

    #[test]
    fn pem_wraps_lines_at_64_characters() {
        let block = pem(&[[0u8; 100]]);
        let lines: Vec<&str> = block.lines().collect();

        assert_eq!(lines.len(), 5);
        assert_eq!(lines[1], "A".repeat(64));
        assert_eq!(lines[2], "A".repeat(64));
        assert_eq!(lines[3], "AAAAAA==");
        assert_eq!(lines[4], "-----END CERTIFICATE-----");
    }

    #[tokio::test]
    async fn write_creates_the_directory_and_the_file() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(BUNDLE_PATH);

        write(&path, "roots".to_string()).await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "roots");
    }

    #[tokio::test]
    async fn write_replaces_a_file_with_other_contents() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(BUNDLE_PATH);
        write(&path, "old roots".to_string()).await.unwrap();

        write(&path, "new roots".to_string()).await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new roots");
        let files = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(files, 1, "the temporary file is left behind");
    }

    #[tokio::test]
    async fn write_fails_when_the_directory_is_a_file() {
        let home = tempfile::tempdir().unwrap();
        let tls = home.path().join("tls");
        std::fs::write(&tls, "not a directory").unwrap();

        let result = write(&home.path().join(BUNDLE_PATH), "roots".to_string()).await;

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&tls).unwrap(), "not a directory");
        let files = std::fs::read_dir(home.path()).unwrap().count();
        assert_eq!(files, 1, "a temporary file is left behind");
    }

    #[tokio::test]
    async fn ensure_writes_the_mozilla_roots() {
        let home = tempfile::tempdir().unwrap();
        let ssl_com = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .find(|cert| {
                cert.windows(28)
                    .any(|name| name == b"SSL.com TLS ECC Root CA 2022")
            })
            .expect("the Mozilla roots include SSL.com TLS ECC Root CA 2022");

        let path = home.path().join(BUNDLE_PATH);

        ensure(&path).await.unwrap();

        let bundle = std::fs::read_to_string(&path).unwrap();
        assert!(bundle.contains(&pem(&[ssl_com])));
    }
}
