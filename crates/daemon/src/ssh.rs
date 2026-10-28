//! SSH access to sandboxes: key provisioning and host names. The daemon's `host_config` module
//! writes the host's SSH config for them.
//!
//! Every sandbox gets a host name like `project.fbk`. The generated SSH config points that
//! name at `fbk ssh-proxy`, which tunnels the connection through the daemon socket to the
//! SSH server microsandbox runs for the sandbox. No sshd, TCP port or DNS record is involved.
//!
//! That server runs every session with the sandbox's shell. [`with_login_shell`] makes that
//! shell a script that starts the session user's login shell with `SHELL` set, as sshd does.

use anyhow::{Context, Result};
use microsandbox::sandbox::{SandboxBuilder, SandboxHandle};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use std::collections::HashSet;
use std::fs;
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Sandbox label that stores the sandbox's SSH host name.
pub const HOSTNAME_LABEL: &str = "firebrick.hostname";

const HOST_SUFFIX: &str = ".fbk";
pub(crate) const CLIENT_KEY_FILE: &str = "id_ed25519";
const CLIENT_PUBLIC_KEY_FILE: &str = "id_ed25519.pub";
const HOST_KEY_FILE: &str = "host_ed25519";
pub(crate) const KNOWN_HOSTS_FILE: &str = "known_hosts";

/// Name of the script that starts the login shell, under `/.msb/scripts` in the guest.
const LOGIN_SCRIPT: &str = "firebrick-login";

/// Content of the [`LOGIN_SCRIPT`]: looks up the session user's shell in `/etc/passwd`, falls
/// back to `/bin/sh`, exports it as `SHELL` and runs it with the session's arguments, or as a
/// login shell for an interactive session.
const LOGIN_SCRIPT_CONTENT: &str = include_str!("firebrick-login.sh");

/// Returns the builder with the [`LOGIN_SCRIPT`] as the sandbox's shell, so SSH sessions run
/// in the session user's login shell with `SHELL` set. microsandbox writes the script at every
/// start.
pub fn with_login_shell(builder: SandboxBuilder) -> SandboxBuilder {
    builder
        .script(LOGIN_SCRIPT, LOGIN_SCRIPT_CONTENT)
        .shell(format!("/.msb/scripts/{LOGIN_SCRIPT}"))
}

/// Returns the host key the sandbox SSH servers identify themselves with.
pub fn host_key_path() -> Result<PathBuf> {
    Ok(ssh_dir()?.join(HOST_KEY_FILE))
}

/// Returns the public key of the client key that may log in to sandboxes.
pub fn client_public_key_path() -> Result<PathBuf> {
    Ok(ssh_dir()?.join(CLIENT_PUBLIC_KEY_FILE))
}

/// Returns the directory holding the SSH keys and config, failing when there's no data
/// directory instead of falling back to a shared one.
pub(crate) fn ssh_dir() -> Result<PathBuf> {
    firebrick_utils::ssh_dir().context(firebrick_utils::NO_DATA_DIR)
}

/// Creates the client and host keys when they're missing and pins the host key for `*.fbk`.
pub fn ensure_keys() -> Result<()> {
    let dir = ssh_dir()?;

    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    // Win32-OpenSSH requires the access list that the keys inherit from the directory.
    firebrick_utils::make_private_dir(&dir)?;

    let client_key = load_or_create_key(&dir.join(CLIENT_KEY_FILE))?;
    write_file(
        &dir.join(CLIENT_PUBLIC_KEY_FILE),
        &format!("{}\n", client_key.public_key().to_openssh()?),
    )?;

    let host_key = load_or_create_key(&dir.join(HOST_KEY_FILE))?;
    write_file(
        &dir.join(KNOWN_HOSTS_FILE),
        &format!("*{HOST_SUFFIX} {}\n", host_key.public_key().to_openssh()?),
    )?;

    Ok(())
}

/// Reads an OpenSSH private key, generating a new ed25519 key when the file doesn't exist.
fn load_or_create_key(path: &Path) -> Result<PrivateKey> {
    match fs::read_to_string(path) {
        Ok(content) => PrivateKey::from_openssh(content)
            .with_context(|| format!("failed to read SSH key {}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let key = PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519)?;

            let mut file = private_file_options()
                .create_new(true)
                .open(path)
                .with_context(|| format!("failed to create SSH key {}", path.display()))?;
            file.write_all(key.to_openssh(LineEnding::LF)?.as_bytes())?;

            Ok(key)
        }
        Err(err) => Err(err).with_context(|| format!("failed to read SSH key {}", path.display())),
    }
}

/// Returns the SSH host name stored on a sandbox, if it has one.
pub fn hostname_of(handle: &SandboxHandle) -> Option<String> {
    handle
        .config()
        .ok()?
        .spec
        .labels
        .get(HOSTNAME_LABEL)
        .cloned()
}

/// Picks a host name for a workspace that none of the `taken` names uses: `<leaf>.fbk`,
/// then `<leaf>-2.fbk`, `<leaf>-3.fbk` and so on.
pub fn pick_hostname(workspace: &str, taken: &HashSet<String>) -> String {
    let base = firebrick_utils::sanitize_label(Path::new(workspace));

    (1..)
        .map(|n| match n {
            1 => format!("{base}{HOST_SUFFIX}"),
            n => format!("{base}-{n}{HOST_SUFFIX}"),
        })
        .find(|hostname| !taken.contains(hostname))
        .expect("there's always a free host name")
}

/// Writes a file that only the user can read, keeping the permissions of an existing file. On
/// Windows a new file inherits the access list of its directory in the user's profile.
fn write_file(path: &Path, content: &str) -> Result<()> {
    let mut file = private_file_options()
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(content.as_bytes())?;

    Ok(())
}

/// Returns options that open a file for writing and create it with mode `0600` on Unix. On
/// Windows a new file inherits the access list of its directory in the user's profile.
fn private_file_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn taken(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn hostname_uses_workspace_leaf() {
        assert_eq!(
            pick_hostname("/home/user/project", &taken(&[])),
            "project.fbk"
        );
    }

    #[test]
    fn hostname_is_sanitized() {
        assert_eq!(
            pick_hostname("/home/user/My_Project.v2--", &taken(&[])),
            "my-project-v2.fbk"
        );
        assert_eq!(pick_hostname("/home/user/__", &taken(&[])), "sandbox.fbk");
        assert_eq!(pick_hostname("/", &taken(&[])), "sandbox.fbk");
    }

    #[test]
    fn hostname_gets_numbered_when_taken() {
        assert_eq!(
            pick_hostname("/a/project", &taken(&["project.fbk"])),
            "project-2.fbk"
        );
        assert_eq!(
            pick_hostname(
                "/a/project",
                &taken(&["project.fbk", "project-2.fbk", "other.fbk"])
            ),
            "project-3.fbk"
        );
    }

    #[test]
    fn created_key_can_be_loaded_again() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("key");

        let created = load_or_create_key(&path).unwrap();
        let loaded = load_or_create_key(&path).unwrap();

        assert_eq!(created.public_key(), loaded.public_key());
        #[cfg(unix)]
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        #[cfg(unix)]
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn login_shell_is_the_sandbox_shell() {
        let builder = microsandbox::Sandbox::builder("login-test").image("ubuntu:26.04");

        let spec = with_login_shell(builder).build().await.unwrap().spec;

        assert_eq!(
            spec.runtime.shell.as_deref(),
            Some("/.msb/scripts/firebrick-login")
        );
        assert!(spec.runtime.scripts.contains_key(LOGIN_SCRIPT));
    }
}
