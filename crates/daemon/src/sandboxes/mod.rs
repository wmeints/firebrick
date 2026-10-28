//! Sandbox management on top of microsandbox: the sandbox lifecycle, the workspace's mise
//! tools, the secrets of sandboxes, their port forwards, their URL relays, their network logs,
//! their clipboard channels, SSH host names, the generated SSH config, the editors' Remote-SSH
//! settings and Zed's remote projects.

mod error;
mod ports;
pub mod provision;
mod recreate;
mod secret_updates;
mod settings;
mod status;

use crate::forward::{ForwardReport, Forwards, SshConnector};
use crate::guest::clipboard::{self, Clipboards, HostClipboard};
use crate::guest::netlog::{NetLogs, Record};
use crate::guest::open::{Opener, Relays};
use crate::host_config;
use crate::sandboxes::provision::env;
use crate::sandboxes::provision::git::{self, GitIdentity};
use crate::sandboxes::provision::mise::{self, MiseError, MiseUpdate};
use crate::sandboxes::provision::network;
use crate::sandboxes::provision::pull::PullUpdate;
use crate::secrets::SecretStore;
use crate::ssh;
use firebrick_spec::{MountSpec, NetworkSpec, PortMapping};
use microsandbox::sandbox::{RootfsSource, SandboxHandle, SandboxSpec, SandboxStatus};
use microsandbox::{MicrosandboxError, Sandbox};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, PoisonError};
use std::time::Duration;
use tokio::sync::OwnedMutexGuard;

pub use error::SandboxError;
use error::{connect_failed, create_failed, remove_failed, stop_before_remove_failed};
pub use recreate::PROBE_LABEL;
use recreate::create_with;
use settings::{
    check_mounts, dotfiles_mount, new_sandbox_settings, sandbox_image, workspace_host_path_in,
    workspace_mount_path, workspace_path_of,
};
use status::{
    current_status, get_sandbox, is_live, list_all_sandboxes, listed_info, retry_while_not_found,
};
pub use status::{list_all, stop_or_kill};

/// How long a sandbox gets to shut down gracefully before it is killed.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Label that records the image reference a sandbox was created from, e.g.
/// `ghcr.io/wmeints/firebrick-base:v0.8.0`.
pub const IMAGE_LABEL: &str = "firebrick.image";

/// The resources a sandbox gets when it is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resources {
    /// Number of vCPUs.
    pub cpus: u8,
    /// Memory in MiB.
    pub memory_mib: u32,
    /// Size of the Docker volume in MiB.
    pub docker_volume_mib: u32,
}

/// A secret to store, and the sandbox it belongs to. It has no `Debug`, so its value can't end
/// up in logs.
#[derive(Clone)]
pub struct NewSecret {
    /// Environment variable that exposes the secret's placeholder in sandboxes.
    pub name: String,
    /// Value of the secret.
    pub value: String,
    /// Hosts that may receive the value. Empty uses the defaults for well-known names.
    pub allowed_hosts: Vec<String>,
    /// Sandbox the secret belongs to, or `None` for a global secret.
    pub sandbox: Option<String>,
}

/// Progress of starting a sandbox, which the start stream reports to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartProgress {
    /// The image of a new sandbox is being downloaded.
    Pull(PullUpdate),
    /// The mise tools of the workspace are being installed.
    Mise(MiseUpdate),
}

/// The sandbox to start, and the workspace and image to create it from when it doesn't exist.
#[derive(Debug, Clone, Copy)]
pub struct StartSandbox<'a> {
    /// Name of the sandbox.
    pub name: &'a str,
    /// Absolute host path of the workspace to mount.
    pub workspace: &'a str,
    /// Image to create the sandbox from, or empty for the default image.
    pub image: &'a str,
    /// Whether the image's `/sbin/init` runs as PID 1 when the sandbox is created.
    pub init: bool,
    /// Whether the sandbox trusts and installs its workspace's mise tools on start. Stored when
    /// the sandbox is created.
    pub mise: bool,
    /// Egress rules of the sandbox, applied when it is created.
    pub network: &'a NetworkSpec,
    /// Host ports to forward to the sandbox. Stored with the sandbox; `None` keeps the stored
    /// list.
    pub ports: Option<&'a [PortMapping]>,
    /// Extra host directories with absolute host paths, mounted when the sandbox is created.
    pub mounts: &'a [MountSpec],
    /// Absolute host path of the directory with the user's mise dotfiles, mounted read-only at
    /// [`firebrick_spec::MISE_DOTFILES_PATH`] when the sandbox is created.
    pub dotfiles: Option<&'a str>,
    /// Whether the sandbox can read the host clipboard. Stored when the sandbox is created.
    pub clipboard: bool,
    /// Environment variables of the sandbox, applied when it's created or starts; `None` keeps
    /// the stored ones. No name is also the name of a secret the sandbox gets.
    pub env: Option<&'a BTreeMap<String, String>>,
    /// The git identity of the sandbox's default user, applied whenever the request leaves the
    /// sandbox running; `None` leaves its git config alone.
    pub git_identity: Option<GitIdentity<'a>>,
}

/// What starting a sandbox did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// The forwards of the sandbox.
    pub forwards: ForwardReport,
    /// Whether the sandbox was already running with other environment variables than the
    /// requested ones, which it gets when it restarts.
    pub env_pending_restart: bool,
}

/// The name, status, SSH host name and workspace paths of a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInfo {
    /// Name of the sandbox.
    pub name: String,
    /// Status of the sandbox, or `None` in a listing when its runtime didn't report whether it's
    /// paused.
    pub status: Option<SandboxStatus>,
    /// SSH host name of the sandbox, if it has one.
    pub hostname: Option<String>,
    /// Guest path the workspace is mounted at, e.g. `/workspaces/project`, if it has one.
    pub workspace_path: Option<String>,
    /// Host directory mounted as the workspace, e.g. `/home/user/project`, if it has one.
    pub workspace_host_path: Option<String>,
    /// Image reference the sandbox was created from, if it's known.
    pub image: Option<String>,
    /// The default image when the sandbox runs another `firebrick-base` image, so an upgrade
    /// moves it to this release's image.
    pub upgrade_image: Option<String>,
}

impl SandboxInfo {
    /// Reads the sandbox's config once and reports it with the status. A config that can't be
    /// read leaves out everything it holds, and is logged.
    fn of(handle: &SandboxHandle, status: Option<SandboxStatus>) -> Self {
        let spec = match handle.config() {
            Ok(config) => Some(config.spec),
            Err(err) => {
                tracing::warn!(
                    "failed to read the config of sandbox {}: {err}",
                    handle.name()
                );
                None
            }
        };
        let spec = spec.as_ref();
        let image = spec.and_then(|spec| recorded_image(&spec.labels, &spec.image));

        Self {
            name: handle.name().to_string(),
            status,
            hostname: spec.and_then(|spec| spec.labels.get(ssh::HOSTNAME_LABEL).cloned()),
            workspace_path: spec.and_then(|spec| spec.runtime.workdir.clone()),
            workspace_host_path: spec.and_then(workspace_host_path_in),
            upgrade_image: image
                .as_deref()
                .and_then(|image| upgrade_image(image, firebrick_spec::DEFAULT_IMAGE)),
            image,
        }
    }
}

/// Returns the image reference a sandbox was created from: its image label, or the OCI
/// reference of its root filesystem for a sandbox created before the label existed.
fn recorded_image(labels: &BTreeMap<String, String>, rootfs: &RootfsSource) -> Option<String> {
    labels.get(IMAGE_LABEL).cloned().or_else(|| match rootfs {
        RootfsSource::Oci(oci) => Some(oci.reference.clone()),
        _ => None,
    })
}

/// Returns `default` when `image` is another tag of the same repository, such as an older
/// `firebrick-base` release, and `None` for the default image itself or any other image.
fn upgrade_image(image: &str, default: &str) -> Option<String> {
    let outdated =
        image != default && repository(image).is_some_and(|repo| repository(default) == Some(repo));

    outdated.then(|| default.to_string())
}

/// Returns the repository of an image reference with a tag, the part before the tag.
fn repository(reference: &str) -> Option<&str> {
    reference.rsplit_once(':').map(|(repository, _)| repository)
}

/// The result of upgrading a sandbox to the latest version of an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgrade {
    /// Whether the sandbox was recreated. False when its image already had the pulled digest.
    pub upgraded: bool,
    /// Image the sandbox was created from before the upgrade.
    pub old_image: String,
    /// Image the sandbox was upgraded to, or is already up to date with.
    pub new_image: String,
}

/// Manages sandboxes, the secrets they get, their port forwards, their URL relays, their
/// network logs and their clipboard channels.
pub struct SandboxManager {
    secrets: SecretStore,
    forwards: Arc<Forwards>,
    relays: Relays,
    netlogs: NetLogs,
    clipboards: Clipboards,
    // Held while secrets are stored or added to sandboxes, so a sandbox that is being created
    // can't miss a secret that is being set, and concurrent sets can't mix up values.
    secrets_lock: tokio::sync::Mutex<()>,
    // One lock per sandbox name, held while the sandbox is started, stopped, removed, connected
    // to, recreated or its ports change, so recreating a sandbox can't interleave with another
    // operation on it.
    sandbox_locks: SandboxLocks,
    // Held while the ports of a sandbox are read or stored and its forwards are reconciled, so a
    // request with stale ports can't undo the forwards of a newer one.
    ports_lock: tokio::sync::Mutex<()>,
    // Held for reading while a sandbox is updated or upgraded, and for writing when the daemon
    // shuts down, so it waits for a recreate instead of leaving the sandbox removed.
    recreations: tokio::sync::RwLock<()>,
}

/// The locks of the sandboxes that are in use, by sandbox name.
type SandboxLocks = std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// Holds the lock of a sandbox. Releasing it removes the lock from the map when no other task
/// waits for it, so the map only holds the locks of sandboxes in use.
struct SandboxGuard<'a> {
    locks: &'a SandboxLocks,
    name: String,
    guard: Option<OwnedMutexGuard<()>>,
}

impl Drop for SandboxGuard<'_> {
    fn drop(&mut self) {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(guard) = self.guard.take() else {
            return;
        };
        // Tasks only take a reference while holding the map, so when the map and this guard
        // hold the only references, no task waits for the lock or can start waiting.
        let unused = Arc::strong_count(OwnedMutexGuard::mutex(&guard)) == 2;
        drop(guard);

        if unused {
            locks.remove(&self.name);
        }
    }
}

impl SandboxManager {
    /// Creates a manager that adds the secrets from `secrets` to sandboxes, opens the URLs from
    /// sandboxes with `opener` and reads the host clipboard for them from `clipboard`. It keeps
    /// network logs and clipboard sockets in the directories the environment selects, see
    /// [`NetLogs::from_env`] and [`Clipboards::from_env`].
    pub fn new(
        secrets: SecretStore,
        opener: Arc<dyn Opener>,
        clipboard: Arc<dyn HostClipboard>,
    ) -> Self {
        let forwards = Arc::new(Forwards::new(SshConnector::default()));

        Self {
            secrets,
            relays: Relays::new(opener, Arc::clone(&forwards)),
            netlogs: NetLogs::from_env(),
            clipboards: Clipboards::from_env(clipboard),
            forwards,
            secrets_lock: tokio::sync::Mutex::new(()),
            sandbox_locks: std::sync::Mutex::new(HashMap::new()),
            ports_lock: tokio::sync::Mutex::new(()),
            recreations: tokio::sync::RwLock::new(()),
        }
    }
    /// Waits for and takes the lock of the sandbox with the name.
    async fn lock_sandbox(&self, name: &str) -> SandboxGuard<'_> {
        let lock = {
            // The map stays consistent when a thread panics while holding it, so a poisoned
            // lock is safe to use.
            let mut locks = self
                .sandbox_locks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);

            locks.entry(name.to_string()).or_default().clone()
        };

        SandboxGuard {
            locks: &self.sandbox_locks,
            name: name.to_string(),
            guard: Some(lock.lock_owned().await),
        }
    }
    /// Starts an existing sandbox or creates a new one when it doesn't exist, installs the
    /// workspace's mise tools when it started, then syncs the SSH config and the editor
    /// settings, also when starting failed. Listens for the sandbox's network log and clipboard
    /// first. Once the sandbox runs, stores the requested ports, opens its forwards and starts
    /// its URL relay, also when it was already running.
    /// `resources` is only called when the sandbox is created. `on_progress` gets the progress
    /// of an image that is downloaded for a new sandbox, and of installing the mise tools.
    /// Fails with `InvalidArgument`, without creating or changing the sandbox, when one of the
    /// requested environment variables has the name of a secret the sandbox gets.
    pub async fn start(
        &self,
        request: StartSandbox<'_>,
        resources: impl FnOnce() -> Result<Resources, SandboxError>,
        mut on_progress: impl FnMut(StartProgress) + Send,
    ) -> Result<Started, SandboxError> {
        let _guard = self.lock_sandbox(request.name).await;
        self.check_env_against_secrets(request)?;

        let result = match Sandbox::get(request.name).await {
            Ok(existing_sb) => {
                self.listen_if_routed(&existing_sb).await;
                start_existing_sandbox(&existing_sb, request, mise_progress(&mut on_progress)).await
            }
            Err(_) => match self.listen_for_new(request).await {
                Ok(()) => self
                    .create_and_install(request, resources, on_progress)
                    .await
                    .map(|()| false),
                Err(err) => Err(err),
            },
        };

        sync_ssh_config().await;
        let env_pending_restart = result?;
        self.set_up_running(request).await;

        Ok(Started {
            forwards: self.open_forwards(request.name, request.ports).await,
            env_pending_restart,
        })
    }
    /// Listens for the network logs and clipboards of the sandboxes that have their routes,
    /// running or not. Used when the daemon starts.
    pub async fn restore_listeners(&self) {
        let sandboxes = match list_all_sandboxes().await {
            Ok(sandboxes) => sandboxes,
            Err(err) => {
                tracing::warn!("failed to restore network logs and clipboards: {err}");
                return;
            }
        };

        for sb in &sandboxes {
            self.listen_if_routed(sb).await;
        }
    }
    /// Listens for the network log of a sandbox that is about to be created, and for its
    /// clipboard when it shares the host clipboard. Fails with `FailedPrecondition` when fbkd
    /// can't listen for the clipboard, because the sandbox's stand-ins couldn't reach it.
    async fn listen_for_new(&self, request: StartSandbox<'_>) -> Result<(), SandboxError> {
        self.netlogs.listen(request.name).await;

        if !request.clipboard {
            return Ok(());
        }

        self.clipboards
            .listen(request.name, true)
            .await
            .map_err(|err| {
                SandboxError::failed_precondition(format!(
                    "can't share the host clipboard with sandbox {}: {err}; set XDG_RUNTIME_DIR to \
                 a private directory, or set clipboard: false in .firebrick.yml",
                    request.name
                ))
            })
    }
    /// Listens for the clipboard of an existing sandbox with the labels, logging an error when
    /// that fails. Only a sandbox with the copy label can write the host clipboard.
    async fn listen_for_clipboard(&self, name: &str, labels: &BTreeMap<String, String>) {
        let copy = clipboard::can_copy(labels);

        if let Err(err) = self.clipboards.listen(name, copy).await {
            tracing::error!("failed to listen for the clipboard of sandbox {name}: {err}");
        }
    }
    /// Listens for the sandbox's network log and clipboard when it has their routes. Sandboxes
    /// created before network logging, or without the clipboard setting, have none.
    async fn listen_if_routed(&self, sb: &SandboxHandle) {
        let config = match sb.config() {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(
                    "failed to read the config of sandbox {}; not listening for its network \
                     log and clipboard: {err}",
                    sb.name()
                );
                return;
            }
        };

        if self.netlogs.has_route(sb.name(), &config.spec) {
            self.netlogs.listen(sb.name()).await;
        }

        self.listen_for_routed_clipboard(sb.name(), &config.spec)
            .await;
    }
    /// Listens for the clipboard of an existing sandbox with the spec when it has its route,
    /// and warns when it shares the host clipboard without one.
    async fn listen_for_routed_clipboard(&self, name: &str, spec: &SandboxSpec) {
        match (
            self.clipboards.has_route(name, spec),
            clipboard::is_enabled(&spec.labels),
        ) {
            (true, _) => self.listen_for_clipboard(name, &spec.labels).await,
            (false, true) => tracing::warn!(
                "sandbox {name} shares the host clipboard but has no route to {:?}, for example \
                 because XDG_RUNTIME_DIR changed; recreate it to use the host clipboard again",
                self.clipboards.socket_path(name)
            ),
            (false, false) => {}
        }
    }
    /// Returns the network activity recorded for the sandbox and its current egress rules.
    /// Fails with `FailedPrecondition` when the sandbox has no network log route, because it
    /// was created before network logging, and with `Internal` when its record file can't be
    /// read or is corrupt.
    pub async fn network_activity(
        &self,
        name: &str,
    ) -> Result<(Record, NetworkSpec), SandboxError> {
        let sb = get_sandbox(name).await?;
        let config = sb.config().map_err(|err| {
            tracing::error!(error = ?err, "failed to read the config of sandbox {name}");
            SandboxError::internal("failed to read the sandbox config")
        })?;

        if !self.netlogs.has_route(name, &config.spec) {
            return Err(SandboxError::failed_precondition(format!(
                "sandbox {name} has no network log; it was created before network logging"
            )));
        }

        let record = self.netlogs.record(name).await.map_err(|err| {
            tracing::error!("failed to read the network record of sandbox {name}: {err}");
            SandboxError::internal(format!("failed to read the network record of {name}"))
        })?;

        Ok((record, network::rules_of(&config.spec)))
    }
    /// Stops listening for the network logs and clipboards of the sandboxes, writing their
    /// network records a last time. Used when the daemon shuts down.
    pub async fn close_listeners(&self) {
        self.netlogs.close().await;
        self.clipboards.close().await;
    }
    /// Stops a running sandbox, killing it when it doesn't shut down within [`STOP_TIMEOUT`],
    /// then closes its forwards and forgets its URL relay.
    pub async fn stop(&self, name: &str) -> Result<(), SandboxError> {
        let _guard = self.lock_sandbox(name).await;
        let sb = self.get_or_close_forwards(name).await?;

        stop_or_kill(&sb, STOP_TIMEOUT).await.map_err(|err| {
            tracing::error!(error = ?err, "failed to stop sandbox {name}");
            SandboxError::internal("failed to stop sandbox")
        })?;

        self.forwards.close(name).await;
        self.relays.forget(name);

        Ok(())
    }
    /// Removes a sandbox, closes its forwards and deletes its network log and clipboard socket,
    /// then syncs the SSH config and the editor settings. A running sandbox is only removed with
    /// `force`, which stops it first like [`SandboxManager::stop`].
    pub async fn remove(&self, name: &str, force: bool) -> Result<(), SandboxError> {
        let _guard = self.lock_sandbox(name).await;
        let sb = self.get_or_close_forwards(name).await?;
        let live = is_live(sb.status_snapshot());

        if force && live {
            stop_or_kill(&sb, STOP_TIMEOUT)
                .await
                .map_err(|err| stop_before_remove_failed(name, &err))?;
        }

        // The sandbox doesn't run anymore, also when removing it fails below.
        if force || !live {
            self.forwards.close(name).await;
            self.relays.forget(name);
        }

        sb.remove().await.map_err(|err| remove_failed(name, err))?;
        self.netlogs.remove(name).await;
        self.clipboards.remove(name).await;

        sync_ssh_config().await;

        self.remove_sandbox_secrets(name).await
    }
    /// Returns the sandbox with the name. Closes its forwards and forgets its URL relay when it
    /// doesn't exist anymore, for example because it was removed without fbkd, so its host
    /// ports are freed.
    async fn get_or_close_forwards(&self, name: &str) -> Result<SandboxHandle, SandboxError> {
        let result = get_sandbox(name).await;

        if let Err(SandboxError::NotFound(_)) = result {
            self.forwards.close(name).await;
            self.relays.forget(name);
        }

        result
    }
    /// Returns the sandbox with the name.
    pub async fn get(&self, name: &str) -> Result<SandboxInfo, SandboxError> {
        let sb = get_sandbox(name).await?;

        Ok(SandboxInfo::of(&sb, Some(current_status(&sb).await?)))
    }
    /// Returns every sandbox.
    pub async fn list(&self) -> Result<Vec<SandboxInfo>, SandboxError> {
        let mut sandboxes = vec![];

        for sb in list_all_sandboxes().await? {
            sandboxes.extend(listed_info(&sb).await?);
        }

        Ok(sandboxes)
    }
    /// Connects to the running sandbox with the name and starts its URL relay.
    pub async fn connect(&self, name: &str) -> Result<Sandbox, SandboxError> {
        let _guard = self.lock_sandbox(name).await;

        let sb = get_sandbox(name)
            .await?
            .connect()
            .await
            .map_err(|err| connect_failed(name, &err))?;

        self.relays.ensure(&sb).await;

        Ok(sb)
    }
    /// Connects to the sandbox with the SSH host name, starting it when needed, opens its
    /// stored forwards and starts its URL relay.
    pub async fn connect_by_hostname(&self, hostname: &str) -> Result<Sandbox, SandboxError> {
        let name = sandbox_name_by_hostname(hostname).await?;
        let _guard = self.lock_sandbox(&name).await;

        // Get the sandbox again, because it may have been recreated while waiting for the lock.
        let handle = get_sandbox(&name).await?;
        self.listen_if_routed(&handle).await;

        let sb = handle.connect_or_start_detached().await.map_err(|err| {
            tracing::error!(error = ?err, "failed to start sandbox with host name {hostname}");
            SandboxError::failed_precondition("failed to start sandbox")
        })?;

        self.open_forwards(sb.name(), None).await;
        self.relays.ensure(&sb).await;

        Ok(sb)
    }
    /// Opens the stored forwards and starts the URL relay of the sandbox when it runs after an
    /// update of its rules. The relay checks URLs against the new rules.
    async fn reopen_forwards_and_relay(&self, name: &str) {
        let running = Sandbox::get(name)
            .await
            .is_ok_and(|sb| sb.status_snapshot() == SandboxStatus::Running);

        if running {
            self.open_forwards(name, None).await;
            self.start_relay(name).await;
        }
    }
    /// Connects to the running sandbox and starts its URL relay unless it has one, logging a
    /// warning when it can't connect.
    async fn start_relay(&self, name: &str) {
        match connect(name).await {
            Ok(sb) => self.relays.ensure(&sb).await,
            Err(err) => tracing::warn!("failed to start the URL relay of sandbox {name}: {err}"),
        }
    }
    /// Connects to the sandbox a start request left running, starts its URL relay unless it has
    /// one and gives it the request's git identity, logging a warning when it can't connect.
    async fn set_up_running(&self, request: StartSandbox<'_>) {
        let sb = match connect(request.name).await {
            Ok(sb) => sb,
            Err(err) => {
                tracing::warn!(
                    "failed to start the URL relay and set the git identity of sandbox {}: {err}",
                    request.name
                );
                return;
            }
        };

        self.relays.ensure(&sb).await;

        if let Some(identity) = request.git_identity {
            git::apply(&sb, identity).await;
        }
    }
    /// Creates a sandbox, then installs its workspace's mise tools when the request enables
    /// mise, passing the progress of both to `on_progress`.
    async fn create_and_install(
        &self,
        request: StartSandbox<'_>,
        resources: impl FnOnce() -> Result<Resources, SandboxError>,
        mut on_progress: impl FnMut(StartProgress) + Send,
    ) -> Result<(), SandboxError> {
        let (sb, guest_path) = self
            .create_sandbox(request, resources, |update| {
                on_progress(StartProgress::Pull(update))
            })
            .await?;

        if request.mise {
            install_mise_tools(&sb, &guest_path, mise_progress(&mut on_progress)).await?;
        }

        Ok(())
    }
    /// Creates a sandbox for the workspace with the stored secrets, passing the progress of its
    /// image pull to `on_pull`. Returns it with the guest path of its workspace.
    async fn create_sandbox(
        &self,
        request: StartSandbox<'_>,
        resources: impl FnOnce() -> Result<Resources, SandboxError>,
        on_pull: impl FnMut(PullUpdate) + Send,
    ) -> Result<(Sandbox, String), SandboxError> {
        let guest_path = workspace_mount_path(request.workspace)?;
        check_mounts(request.mounts, &guest_path)?;
        let dotfiles = dotfiles_mount(request.dotfiles)?;
        let resources = resources()?;
        let hostname = ssh::pick_hostname(request.workspace, &taken_hostnames().await?);
        let _secrets_guard = self.secrets_lock.lock().await;
        let secrets = self.load_secrets_for(request.name)?;

        let mut settings = new_sandbox_settings(request, &guest_path, &hostname, resources);
        settings.mounts.extend(dotfiles);
        let builder = settings
            .builder(&self.netlogs, &self.clipboards)
            .image(sandbox_image(request.image));

        let sb = match create_with(builder, &secrets, request.network, on_pull).await {
            Ok(sb) => sb,
            Err(err) => {
                tracing::error!(error = ?err, "failed to create sandbox {}", request.name);
                return Err(create_failed(request.name, sandbox_image(request.image), &err).await);
            }
        };

        tracing::info!("created sandbox {} as {hostname}", request.name);

        Ok((sb, guest_path))
    }
}

/// Returns the name of the sandbox with the SSH host name.
async fn sandbox_name_by_hostname(hostname: &str) -> Result<String, SandboxError> {
    retry_while_not_found(|| Sandbox::list_with(|opt| opt.label(ssh::HOSTNAME_LABEL, hostname)))
        .await
        .map_err(|err| {
            tracing::error!(error = ?err, "failed to list sandboxes with host name {hostname}");
            SandboxError::internal("failed to list sandboxes")
        })?
        .sandboxes
        .first()
        .map(|handle| handle.name().to_string())
        .ok_or_else(|| SandboxError::not_found("couldn't find a sandbox with that host name"))
}

/// Returns the image reference of an OCI root filesystem, or `None` for another one.
fn image_reference(image: RootfsSource) -> Option<String> {
    match image {
        RootfsSource::Oci(oci) => Some(oci.reference),
        _ => None,
    }
}

/// Starts an existing sandbox, first giving it a host name when it has none and the requested
/// environment variables, then installs its workspace's mise tools when the sandbox has mise
/// enabled, passing their progress to `on_mise`. Doesn't change a sandbox that is already
/// running or starting, and returns whether it runs with other variables than the requested
/// ones.
async fn start_existing_sandbox(
    sb: &SandboxHandle,
    request: StartSandbox<'_>,
    on_mise: impl FnMut(MiseUpdate) + Send,
) -> Result<bool, SandboxError> {
    // Sandboxes created before SSH support have no host name yet.
    if ssh::hostname_of(sb).is_none() {
        assign_hostname(sb, request).await?;
    }

    if matches!(
        sb.status_snapshot(),
        SandboxStatus::Running | SandboxStatus::Starting
    ) {
        return env_pending_restart(sb, request);
    }

    if let Some(wanted) = request.env {
        sync_env(sb, wanted).await?;
    }

    let started = match sb.start_detached().await {
        Ok(started) => started,
        // Another request started the sandbox after its status was read, and installs the tools.
        // It may have booted it before the sync, so report a change against the config `sb`
        // read before it.
        Err(MicrosandboxError::SandboxStillRunning(_)) => {
            return env_pending_restart(sb, request);
        }
        Err(err) => {
            tracing::error!(error = ?err, "failed to start sandbox {}", request.name);
            return Err(SandboxError::internal("failed to start sandbox"));
        }
    };

    tracing::info!("started sandbox {}", request.name);
    install_tools_of_existing(sb, &started, on_mise).await?;

    Ok(false)
}

/// Installs the workspace's mise tools in the started sandbox when it has mise enabled.
async fn install_tools_of_existing(
    sb: &SandboxHandle,
    started: &Sandbox,
    on_mise: impl FnMut(MiseUpdate) + Send,
) -> Result<(), SandboxError> {
    match workspace_path_of(sb) {
        Some(workspace) if mise_enabled(sb) => {
            install_mise_tools(started, &workspace, on_mise).await
        }
        _ => Ok(()),
    }
}

/// Whether the sandbox got a variable with the name from its spec. A sandbox whose config
/// can't be read counts as not having it, with a warning: adding the secret to it fails and
/// reports the sandbox anyway.
fn sets_variable(sb: &SandboxHandle, name: &str) -> bool {
    match sb.config() {
        Ok(config) => env::names_variable(&config.spec, name),
        Err(err) => {
            tracing::warn!(error = ?err, "failed to read the environment of sandbox {}", sb.name());
            false
        }
    }
}

/// Whether the running sandbox has other environment variables than the request asks for. A
/// request without variables keeps the stored ones, so it has none pending.
fn env_pending_restart(
    sb: &SandboxHandle,
    request: StartSandbox<'_>,
) -> Result<bool, SandboxError> {
    let Some(wanted) = request.env else {
        return Ok(false);
    };

    env::differs(sb, wanted).map_err(|err| {
        tracing::error!(error = ?err, "failed to read the environment of sandbox {}", sb.name());
        SandboxError::internal(format!(
            "failed to read the environment variables of sandbox {}",
            sb.name()
        ))
    })
}

/// Gives a stopped sandbox the environment variables from its next start on.
async fn sync_env(
    sb: &SandboxHandle,
    wanted: &BTreeMap<String, String>,
) -> Result<(), SandboxError> {
    env::sync(sb, wanted).await.map_err(|err| {
        tracing::error!(error = ?err, "failed to set the environment of sandbox {}", sb.name());
        SandboxError::internal(format!(
            "failed to set the environment variables of sandbox {}",
            sb.name()
        ))
    })
}

/// Whether the sandbox installs its workspace's mise tools on start.
fn mise_enabled(sb: &SandboxHandle) -> bool {
    sb.config()
        .is_ok_and(|config| mise::is_enabled(&config.spec.labels))
}

/// Passes the progress of installing the mise tools on to `on_progress` as start progress.
fn mise_progress(
    on_progress: &mut (impl FnMut(StartProgress) + Send),
) -> impl FnMut(MiseUpdate) + Send + '_ {
    move |update| on_progress(StartProgress::Mise(update))
}

/// Trusts and installs the mise tools of the workspace, passing the progress to `on_progress`.
/// An image without mise only logs a warning, because mise is a convenience the developer of a
/// custom image may leave out.
async fn install_mise_tools(
    sb: &Sandbox,
    workspace: &str,
    on_progress: impl FnMut(MiseUpdate) + Send,
) -> Result<(), SandboxError> {
    match mise::install_tools(sb, workspace, on_progress).await {
        Ok(()) => Ok(()),
        Err(err @ MiseError::Failed { .. }) => {
            tracing::warn!("{err}");
            Err(SandboxError::failed_precondition(err.to_string()))
        }
        Err(err @ MiseError::NotInstalled(_)) => {
            tracing::warn!("skipped installing mise tools: {err}");
            Ok(())
        }
        Err(err @ MiseError::Exec { .. }) => {
            tracing::error!(error = ?err, "failed to install mise tools in {}", sb.name());
            Err(SandboxError::internal("failed to install mise tools"))
        }
    }
}

/// Gives the sandbox a host name based on its workspace, logging a warning when that fails.
/// Fails when the host names other sandboxes use can't be listed.
async fn assign_hostname(
    sb: &SandboxHandle,
    request: StartSandbox<'_>,
) -> Result<(), SandboxError> {
    let workspace = match request.workspace {
        "" => request.name,
        workspace => workspace,
    };
    let hostname = ssh::pick_hostname(workspace, &taken_hostnames().await?);

    let result = sb
        .modify()
        .label(ssh::HOSTNAME_LABEL, &hostname)
        .apply()
        .await;

    if let Err(err) = result {
        tracing::warn!("failed to assign host name to {}: {err}", request.name);
    }

    Ok(())
}

/// Connects to the running sandbox.
async fn connect(name: &str) -> Result<Sandbox, MicrosandboxError> {
    Sandbox::get(name).await?.connect().await
}

/// Returns the SSH host names that sandboxes already use.
async fn taken_hostnames() -> Result<HashSet<String>, SandboxError> {
    Ok(list_all_sandboxes()
        .await?
        .iter()
        .filter_map(ssh::hostname_of)
        .collect())
}

/// Regenerates the SSH config, the editors' Remote-SSH settings and Zed's remote projects from
/// the current sandboxes, logging a warning when that fails. SSH access is a convenience, so a
/// failure here doesn't fail the request.
pub async fn sync_ssh_config() {
    match list_all_sandboxes().await {
        Ok(sandboxes) => {
            host_config::sync(&sandboxes.iter().filter_map(ssh_host).collect::<Vec<_>>())
        }
        Err(err) => tracing::warn!("failed to update SSH config: {err}"),
    }
}

/// Returns the sandbox as the host's config files list it, or `None` when it has no host name.
fn ssh_host(handle: &SandboxHandle) -> Option<host_config::Host> {
    Some(host_config::Host {
        hostname: ssh::hostname_of(handle)?,
        sandbox: handle.name().to_string(),
        workspace_path: workspace_path_of(handle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::clipboard::HostTools;
    use crate::guest::open::HostOpener;
    use crate::secrets::Secret;
    use std::path::Path;

    /// Returns the host path as an absolute path of the platform: unchanged on Unix, and on the
    /// `C:` drive on Windows, where a path without a drive isn't absolute.
    pub(super) fn host(path: &str) -> String {
        if cfg!(windows) {
            format!("C:{path}")
        } else {
            path.to_string()
        }
    }

    /// Returns a manager with a store at `path` that has a secret for `example.com` under each
    /// name.
    fn manager_with_secrets(path: &Path, names: &[&str]) -> SandboxManager {
        let store = SecretStore::new(path);

        for name in names {
            let secret =
                Secret::new(name.to_string(), "value".into(), vec!["example.com".into()]).unwrap();
            store.set(secret).unwrap();
        }

        SandboxManager::new(
            store,
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        )
    }

    /// Returns a secret for `example.com` with the name, in the sandbox's scope or global.
    pub(super) fn scoped_secret(name: &str, sandbox: Option<&str>) -> Secret {
        Secret::new(name.to_string(), "value".into(), vec!["example.com".into()])
            .unwrap()
            .in_scope(sandbox.map(str::to_string))
    }

    /// Polls the future once and returns whether it's still pending.
    async fn poll_once_pending<F: Future>(mut future: std::pin::Pin<&mut F>) -> bool {
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx).is_pending()))
            .await
    }

    fn lock_count(manager: &SandboxManager) -> usize {
        manager.sandbox_locks.lock().unwrap().len()
    }

    #[tokio::test]
    async fn released_sandbox_lock_is_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        let manager = manager_with_secrets(&dir.path().join("secrets.yml"), &[]);

        let guard = manager.lock_sandbox("fbk-unit-lock").await;
        assert_eq!(lock_count(&manager), 1);
        drop(guard);

        assert_eq!(lock_count(&manager), 0);
    }

    #[tokio::test]
    async fn sandbox_lock_is_kept_while_a_task_waits_for_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let manager = manager_with_secrets(&dir.path().join("secrets.yml"), &[]);

        let first = manager.lock_sandbox("fbk-unit-lock").await;
        let second = manager.lock_sandbox("fbk-unit-lock");
        tokio::pin!(second);
        // Polls the waiting task once, so it takes its reference to the lock and waits.
        assert!(poll_once_pending(second.as_mut()).await);
        drop(first);
        assert_eq!(lock_count(&manager), 1);

        drop(second.await);
        assert_eq!(lock_count(&manager), 0);
    }

    const DEFAULT: &str = "ghcr.io/wmeints/firebrick-base:v0.8.0";

    #[test]
    fn default_image_is_not_outdated() {
        assert_eq!(upgrade_image(DEFAULT, DEFAULT), None);
    }

    #[test]
    fn older_firebrick_base_tag_is_outdated() {
        assert_eq!(
            upgrade_image("ghcr.io/wmeints/firebrick-base:v0.7.0", DEFAULT),
            Some(DEFAULT.to_string())
        );
    }

    #[test]
    fn custom_image_is_never_outdated() {
        assert_eq!(upgrade_image("alpine:3.22", DEFAULT), None);
        assert_eq!(
            upgrade_image("ghcr.io/acme/firebrick-base:v0.7.0", DEFAULT),
            None
        );
        assert_eq!(
            upgrade_image("ghcr.io/wmeints/firebrick-base", DEFAULT),
            None
        );
    }

    fn oci(reference: &str) -> RootfsSource {
        RootfsSource::Oci(microsandbox::sandbox::OciRootfsSource {
            reference: reference.to_string(),
            root_disk: None,
        })
    }

    #[test]
    fn recorded_image_prefers_the_image_label() {
        let labels = BTreeMap::from([(IMAGE_LABEL.to_string(), DEFAULT.to_string())]);

        assert_eq!(
            recorded_image(&labels, &oci("ghcr.io/wmeints/firebrick-base:v0.7.0")),
            Some(DEFAULT.to_string())
        );
    }

    #[test]
    fn recorded_image_falls_back_to_the_oci_reference_without_a_label() {
        let old = "ghcr.io/wmeints/firebrick-base:v0.7.0";

        assert_eq!(
            recorded_image(&BTreeMap::new(), &oci(old)),
            Some(old.to_string())
        );
    }

    #[test]
    fn unknown_image_reference_is_not_recorded() {
        let rootfs = RootfsSource::Bind {
            path: "/tmp/rootfs".into(),
            follow_root_symlinks: false,
        };

        assert_eq!(recorded_image(&BTreeMap::new(), &rootfs), None);
    }

    #[test]
    fn list_secrets_sorts_by_name_then_scope_with_global_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path().join("secrets.yml"));
        let entries = [
            ("B", Some("a")),
            ("B", None),
            ("A", Some("z")),
            ("A", None),
            ("A", Some("b")),
        ];
        for (name, sandbox) in entries {
            store.set(scoped_secret(name, sandbox)).unwrap();
        }
        let manager = SandboxManager::new(
            store,
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        );

        let listed = manager.list_secrets().unwrap();

        let expected = [
            ("A", None),
            ("A", Some("b")),
            ("A", Some("z")),
            ("B", None),
            ("B", Some("a")),
        ]
        .map(|(name, sandbox)| scoped_secret(name, sandbox));
        assert_eq!(listed, expected);
    }

    #[tokio::test]
    async fn set_secret_returns_not_found_for_missing_sandbox_without_storing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.yml");
        let manager = SandboxManager::new(
            SecretStore::new(&path),
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        );
        let sandbox = format!("fbk-unit-missing-{}", std::process::id());

        let result = manager
            .set_secret(NewSecret {
                name: "X".into(),
                value: "value".into(),
                allowed_hosts: vec!["example.com".into()],
                sandbox: Some(sandbox.clone()),
            })
            .await;

        assert_eq!(
            result,
            Err(SandboxError::NotFound(format!(
                "sandbox {sandbox} doesn't exist"
            )))
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn remove_secret_returns_not_found_for_missing_sandbox_and_keeps_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.yml");
        let manager = manager_with_secrets(&path, &["X"]);
        let sandbox = format!("fbk-unit-missing-{}", std::process::id());

        let result = manager.remove_secret("X", Some(&sandbox)).await;

        assert_eq!(
            result,
            Err(SandboxError::NotFound(format!(
                "sandbox {sandbox} doesn't exist"
            )))
        );
        assert_eq!(SecretStore::new(&path).load().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_secret_rejects_invalid_secret_without_storing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.yml");
        let manager = SandboxManager::new(
            SecretStore::new(&path),
            Arc::new(HostOpener),
            Arc::new(HostTools::new(None, None)),
        );

        let result = manager
            .set_secret(NewSecret {
                name: "NOT-A-NAME".into(),
                value: "value".into(),
                allowed_hosts: vec![],
                sandbox: None,
            })
            .await;

        assert!(matches!(result, Err(SandboxError::InvalidArgument(_))));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn remove_secret_returns_not_found_and_keeps_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.yml");
        let manager = manager_with_secrets(&path, &["A"]);

        let result = manager.remove_secret("B", None).await;

        assert_eq!(
            result,
            Err(SandboxError::NotFound("secret B doesn't exist".into()))
        );
        assert_eq!(SecretStore::new(&path).load().unwrap().len(), 1);
    }
}
