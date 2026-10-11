//! SSH access to sandboxes: key provisioning, host names and the host's SSH config.
//!
//! Every sandbox gets a host name like `project.fbk`. The generated SSH config points that
//! name at `fbk ssh-proxy`, which tunnels the connection through the daemon socket to the
//! SSH server microsandbox runs for the sandbox. No sshd, TCP port or DNS record is involved.

use anyhow::{Context, Result};
use microsandbox::sandbox::SandboxHandle;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use std::collections::HashSet;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Sandbox label that stores the sandbox's SSH host name.
pub const HOSTNAME_LABEL: &str = "firebrick.hostname";

const HOST_SUFFIX: &str = ".fbk";
const CLIENT_KEY_FILE: &str = "id_ed25519";
const CLIENT_PUBLIC_KEY_FILE: &str = "id_ed25519.pub";
const HOST_KEY_FILE: &str = "host_ed25519";
const KNOWN_HOSTS_FILE: &str = "known_hosts";
const CONFIG_FILE: &str = "config";
const CLI_BINARY: &str = "fbk";

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
fn ssh_dir() -> Result<PathBuf> {
    firebrick_utils::ssh_dir().context(firebrick_utils::NO_DATA_DIR)
}

/// Creates the client and host keys when they're missing and pins the host key for `*.fbk`.
pub fn ensure_keys() -> Result<()> {
    let dir = ssh_dir()?;

    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;

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

            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
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

/// Rewrites the generated SSH config for the given host names and makes sure the user's
/// `~/.ssh/config` includes it.
pub fn sync_config(hostnames: &[String]) -> Result<()> {
    let dir = ssh_dir()?;
    let config_path = dir.join(CONFIG_FILE);

    // Write a temporary file and rename it, so ssh never reads a half-written config.
    let temp_path = dir.join(format!("{CONFIG_FILE}.tmp"));
    write_file(&temp_path, &render_config(hostnames, &dir, &cli_binary()))?;
    fs::rename(&temp_path, &config_path)?;

    let home = std::env::var_os("HOME").context("HOME is not set")?;
    ensure_include(&PathBuf::from(home).join(".ssh/config"), &config_path)
}

/// Renders a host block per sandbox that tunnels through `fbk ssh-proxy`.
fn render_config(hostnames: &[String], ssh_dir: &Path, cli: &Path) -> String {
    let mut config =
        String::from("# Generated by firebrick. Changes to this file are overwritten.\n");

    let mut hostnames = hostnames.to_vec();
    hostnames.sort();

    for hostname in hostnames {
        config.push_str(&format!(
            "\nHost {hostname}\n  \
             User agent\n  \
             ProxyCommand \"{cli}\" ssh-proxy %n\n  \
             IdentityFile \"{identity}\"\n  \
             IdentitiesOnly yes\n  \
             UserKnownHostsFile \"{known_hosts}\"\n  \
             StrictHostKeyChecking yes\n",
            cli = cli.display(),
            identity = ssh_dir.join(CLIENT_KEY_FILE).display(),
            known_hosts = ssh_dir.join(KNOWN_HOSTS_FILE).display(),
        ));
    }

    config
}

/// Adds an `Include` line for the generated config to the top of the user's SSH config,
/// unless it's already there. The include must come before any `Host` block to apply.
fn ensure_include(user_config: &Path, include: &Path) -> Result<()> {
    let include_line = format!("Include \"{}\"", include.display());

    let existing = match fs::read_to_string(user_config) {
        Ok(content) => content,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if let Some(dir) = user_config.parent() {
                fs::create_dir_all(dir)?;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
            String::new()
        }
        Err(err) => return Err(err.into()),
    };

    if existing.lines().any(|line| line.trim() == include_line) {
        return Ok(());
    }

    // Write in place rather than renaming, so a symlinked config (dotfiles) stays a symlink.
    write_file(user_config, &format!("{include_line}\n\n{existing}"))
        .with_context(|| format!("failed to update {}", user_config.display()))
}

/// Writes a file that only the user can read, keeping the permissions of an existing file.
fn write_file(path: &Path, content: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(content.as_bytes())?;

    Ok(())
}

/// Returns the CLI binary next to the daemon, or falls back to looking it up on `PATH`.
fn cli_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(CLI_BINARY)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(CLI_BINARY))
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn config_has_block_per_host() {
        let config = render_config(
            &["web.fbk".to_string(), "api.fbk".to_string()],
            Path::new("/home/user/.local/share/firebrick/ssh"),
            Path::new("/usr/bin/fbk"),
        );

        assert!(config.find("Host api.fbk").unwrap() < config.find("Host web.fbk").unwrap());
        assert!(config.contains("User agent\n"));
        assert!(config.contains("ProxyCommand \"/usr/bin/fbk\" ssh-proxy %n\n"));
        assert!(
            config.contains("IdentityFile \"/home/user/.local/share/firebrick/ssh/id_ed25519\"\n")
        );
        assert!(config.contains(
            "UserKnownHostsFile \"/home/user/.local/share/firebrick/ssh/known_hosts\"\n"
        ));
    }

    #[test]
    fn include_is_prepended_once() {
        let dir = TempDir::new().unwrap();
        let user_config = dir.path().join(".ssh/config");
        let include = Path::new("/data/firebrick/ssh/config");

        fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        fs::write(&user_config, "Host github.com\n  User git\n").unwrap();

        ensure_include(&user_config, include).unwrap();
        ensure_include(&user_config, include).unwrap();

        assert_eq!(
            fs::read_to_string(&user_config).unwrap(),
            "Include \"/data/firebrick/ssh/config\"\n\nHost github.com\n  User git\n"
        );
    }

    #[test]
    fn include_creates_missing_config() {
        let dir = TempDir::new().unwrap();
        let user_config = dir.path().join(".ssh/config");

        ensure_include(&user_config, Path::new("/data/firebrick/ssh/config")).unwrap();

        let mode = fs::metadata(&user_config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            fs::read_to_string(&user_config)
                .unwrap()
                .starts_with("Include \"/data/firebrick/ssh/config\"\n")
        );
    }

    #[test]
    fn created_key_can_be_loaded_again() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("key");

        let created = load_or_create_key(&path).unwrap();
        let loaded = load_or_create_key(&path).unwrap();

        assert_eq!(created.public_key(), loaded.public_key());
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
