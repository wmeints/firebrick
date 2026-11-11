//! The host's own config files that list the sandboxes: the SSH config that points their host
//! names at `fbk ssh-proxy`, VS Code's Remote-SSH settings and Zed's remote projects. [`sync`]
//! rewrites all of them from the current sandboxes.

mod settings_file;
mod ssh_config;
mod vscode;
mod zed;

/// Creates `link` as a symlink to the file `target`, which may not exist, for the tests of
/// files that keep a symlink. Returns `false` when Windows refuses, because creating a symlink
/// takes administrator rights or Developer Mode there, so the test can stop. With
/// `FIREBRICK_TEST_SYMLINKS` set, as in CI, that fails the test instead.
#[cfg(test)]
pub(crate) fn test_symlink(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file(target, link);

    match created {
        Ok(()) => true,
        Err(err) if cfg!(windows) && std::env::var_os("FIREBRICK_TEST_SYMLINKS").is_none() => {
            eprintln!(
                "can't create a symlink ({err}), so the test stops; set \
                 FIREBRICK_TEST_SYMLINKS=1 to fail it"
            );
            false
        }
        Err(err) => panic!("can't create a symlink: {err}"),
    }
}

/// A sandbox that is reachable over SSH, as the host's config files list it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Host {
    /// The SSH host name of the sandbox, such as `project.fbk`.
    pub hostname: String,
    /// The name of the sandbox, which Zed shows for the remote project.
    pub sandbox: String,
    /// The guest path of the sandbox's workspace, such as `/workspaces/project`, which Zed
    /// opens. `None` for a sandbox without a workspace.
    pub workspace_path: Option<String>,
}

/// Regenerates the SSH config, the editors' Remote-SSH settings and Zed's remote projects for
/// the given hosts, logging a warning for each one that fails. SSH access is a convenience, so
/// a failure here doesn't fail the request that triggered the sync.
pub(crate) fn sync(hosts: &[Host]) {
    let hostnames: Vec<&str> = hosts.iter().map(|host| host.hostname.as_str()).collect();

    if let Err(err) = ssh_config::sync_config(&hostnames) {
        tracing::warn!("failed to update SSH config: {err:#}");
    }

    sync_editor_settings(&hostnames);
    sync_zed_settings(hosts);
}

/// Syncs the host names into the editors' Remote-SSH settings, logging a warning per editor
/// that fails.
fn sync_editor_settings(hostnames: &[&str]) {
    let Some(config_root) = vscode::config_root() else {
        tracing::warn!("failed to update editor settings: HOME is not set");
        return;
    };

    for err in vscode::sync_settings(&config_root, hostnames) {
        tracing::warn!(
            "failed to update editor settings: {:#}",
            anyhow::Error::from(err)
        );
    }
}

/// Syncs the hosts into Zed's remote projects, logging a warning when that fails.
fn sync_zed_settings(hosts: &[Host]) {
    let Some(config_root) = zed::config_root() else {
        tracing::warn!("failed to update Zed settings: HOME is not set");
        return;
    };

    if let Err(err) = zed::sync_settings(&config_root, hosts) {
        tracing::warn!(
            "failed to update Zed settings: {:#}",
            anyhow::Error::from(err)
        );
    }
}
