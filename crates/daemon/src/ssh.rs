//! SSH access to sandboxes: key provisioning, host names and the host's SSH config.
//!
//! Every sandbox gets a host name like `project.fbk`. The generated SSH config points that
//! name at `fbk ssh-proxy`, which tunnels the connection through the daemon socket to the
//! SSH server microsandbox runs for the sandbox. No sshd, TCP port or DNS record is involved.
//!
//! That server runs every session with the sandbox's shell. [`with_login_shell`] makes that
//! shell a script that starts the session user's login shell with `SHELL` set, as sshd does.

use crate::atomic_file;
use anyhow::{Context, Result, bail};
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
const CLIENT_KEY_FILE: &str = "id_ed25519";
const CLIENT_PUBLIC_KEY_FILE: &str = "id_ed25519.pub";
const HOST_KEY_FILE: &str = "host_ed25519";
const KNOWN_HOSTS_FILE: &str = "known_hosts";
const CONFIG_FILE: &str = "config";
const CLI_BINARY: &str = "fbk";

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
fn ssh_dir() -> Result<PathBuf> {
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

/// Rewrites the generated SSH config for the given host names and makes sure the user's
/// `~/.ssh/config` includes it.
pub fn sync_config(hostnames: &[String]) -> Result<()> {
    let dir = ssh_dir()?;
    let config_path = dir.join(CONFIG_FILE);

    let config = render_config(hostnames, &dir, &cli_binary());
    atomic_file::write(&config_path, &config, 0o600)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    let home_var = firebrick_utils::HOME_VAR;
    let home = std::env::var_os(home_var).with_context(|| format!("{home_var} is not set"))?;
    ensure_include(
        &PathBuf::from(home).join(".ssh").join("config"),
        &config_path,
    )
}

/// Returns the path as it goes into an SSH config. On Windows that's with forward slashes,
/// which Win32-OpenSSH accepts and which aren't escapes inside quotes.
fn config_path(path: &Path) -> String {
    let path = path.display().to_string();

    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path
    }
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
            cli = config_path(cli),
            identity = config_path(&ssh_dir.join(CLIENT_KEY_FILE)),
            known_hosts = config_path(&ssh_dir.join(KNOWN_HOSTS_FILE)),
        ));
    }

    config
}

/// Adds an `Include` line for the generated config to the top of the user's SSH config,
/// unless it's already there. The include must come before any `Host` block to apply.
fn ensure_include(user_config: &Path, include: &Path) -> Result<()> {
    let include_line = format!("Include \"{}\"", config_path(include));

    let existing = read_user_config(user_config)?;

    if existing.lines().any(|line| line.trim() == include_line) {
        return Ok(());
    }

    atomic_file::write(user_config, &format!("{include_line}\n\n{existing}"), 0o600)
        .with_context(|| format!("failed to update {}", user_config.display()))
}

/// Reads the user's SSH config, creating `~/.ssh` when the config doesn't exist yet. A symlink
/// to a missing file is an error, so the rename doesn't replace the link.
fn read_user_config(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(err) if err.kind() == ErrorKind::NotFound && path.is_symlink() => bail!(
            "{} is a symlink to a missing file; create the target or remove the link",
            path.display()
        ),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
                firebrick_utils::make_private_dir(dir)?;
            }
            Ok(String::new())
        }
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
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

/// Returns the CLI binary next to the daemon, or falls back to looking it up on `PATH`.
fn cli_binary() -> PathBuf {
    let file_name = format!("{CLI_BINARY}{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&file_name)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(CLI_BINARY))
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

        #[cfg(unix)]
        let mode = fs::metadata(&user_config).unwrap().permissions().mode();
        #[cfg(unix)]
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            fs::read_to_string(&user_config)
                .unwrap()
                .starts_with("Include \"/data/firebrick/ssh/config\"\n")
        );
    }

    #[test]
    fn failed_write_leaves_config_unchanged() {
        let dir = TempDir::new().unwrap();
        let ssh_dir = dir.path().join(".ssh");
        let user_config = ssh_dir.join("config");

        fs::create_dir_all(&ssh_dir).unwrap();
        fs::write(&user_config, "Host github.com\n").unwrap();
        // A directory in the place of the temporary file fails the write on every platform,
        // also for root, like a full disk would.
        fs::create_dir(ssh_dir.join(".config.firebrick-tmp")).unwrap();

        let result = ensure_include(&user_config, Path::new("/data/firebrick/ssh/config"));

        let err = result.unwrap_err().to_string();
        assert!(err.starts_with("failed to update"), "{err}");
        assert_eq!(
            fs::read_to_string(&user_config).unwrap(),
            "Host github.com\n"
        );
    }

    #[test]
    fn symlinked_config_stays_symlink() {
        let dir = TempDir::new().unwrap();
        let dotfiles = dir.path().join("dotfiles/ssh_config");
        let user_config = dir.path().join(".ssh/config");

        fs::create_dir_all(dotfiles.parent().unwrap()).unwrap();
        fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        fs::write(&dotfiles, "Host github.com\n").unwrap();
        if !crate::atomic_file::test_symlink(&dotfiles, &user_config) {
            return;
        }

        ensure_include(&user_config, Path::new("/data/firebrick/ssh/config")).unwrap();

        assert!(user_config.is_symlink());
        assert_eq!(
            fs::read_to_string(&dotfiles).unwrap(),
            "Include \"/data/firebrick/ssh/config\"\n\nHost github.com\n"
        );
    }

    #[test]
    fn dangling_symlink_is_left_alone() {
        let dir = TempDir::new().unwrap();
        let user_config = dir.path().join(".ssh/config");

        fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        let missing = dir.path().join("missing");
        if !crate::atomic_file::test_symlink(&missing, &user_config) {
            return;
        }

        let err = ensure_include(&user_config, Path::new("/data/firebrick/ssh/config"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("is a symlink to a missing file"), "{err}");
        assert_eq!(fs::read_link(&user_config).unwrap(), missing);
        assert!(!missing.exists());
    }

    #[test]
    #[cfg(unix)]
    fn existing_config_keeps_mode() {
        let dir = TempDir::new().unwrap();
        let user_config = dir.path().join(".ssh/config");

        fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        fs::write(&user_config, "Host github.com\n").unwrap();
        fs::set_permissions(&user_config, fs::Permissions::from_mode(0o640)).unwrap();

        ensure_include(&user_config, Path::new("/data/firebrick/ssh/config")).unwrap();

        #[cfg(unix)]
        let mode = fs::metadata(&user_config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
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
