//! Sandbox management on top of microsandbox: the sandbox lifecycle, the workspace's mise
//! tools, the secrets of sandboxes, their port forwards, their URL relays, their network logs,
//! their clipboard channels, SSH host names, the generated SSH config, the editors' Remote-SSH
//! settings and Zed's remote projects.

use crate::clipboard::{self, Clipboards, HostClipboard};
use crate::env;
use crate::forward::{self, ForwardReport, Forwards, SshConnector};
use crate::git::{self, GitIdentity};
use crate::mise::{self, MiseError, MiseUpdate};
use crate::netlog::{NetLogs, Record};
use crate::network;
use crate::open::{Opener, Relays};
use crate::pull::{self, PullUpdate};
use crate::secrets::{self, Secret, SecretStore};
use crate::ssh;
use crate::vscode;
use crate::zed;
use firebrick_spec::{MountSpec, NetworkSpec, PortMapping, VolumesSpec, with_port};
use microsandbox::sandbox::{
    HostPermissions, MountBuilder, OwnedVolumeStorage, PullPolicy, RootfsSource, SandboxBuilder,
    SandboxHandle, SandboxSpec, SandboxStatus, VolumeMount,
};
use microsandbox::snapshot::Snapshot;
use microsandbox::{MicrosandboxError, Sandbox};
use microsandbox_image::ImageError;
use oci_client::errors::{OciDistributionError, OciEnvelope, OciErrorCode};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::OwnedMutexGuard;
use tokio::time::{Instant, sleep, timeout};

/// How long a sandbox gets to shut down gracefully before it is killed.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the runtime of a running sandbox gets to report whether the sandbox is paused.
const PAUSE_STATE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait before asking the runtime of a sandbox again whether it's paused.
const PAUSE_STATE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// How long deleting a snapshot waits for the operations that still read it.
const SNAPSHOT_IN_USE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long deleting a snapshot that's in use waits before it tries again.
const SNAPSHOT_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Guest path of the disk that holds Docker's data.
const DOCKER_DATA_PATH: &str = "/var/lib/docker";

/// Label that records the image reference a sandbox was created from, e.g.
/// `ghcr.io/wmeints/firebrick-base:v0.8.0`.
pub const IMAGE_LABEL: &str = "firebrick.image";

/// Errors of sandbox management. Each variant carries the message the client sees.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// The sandbox or secret doesn't exist.
    #[error("{0}")]
    NotFound(String),
    /// The request has an invalid value.
    #[error("{0}")]
    InvalidArgument(String),
    /// The sandbox isn't in a state that allows the operation.
    #[error("{0}")]
    FailedPrecondition(String),
    /// A service the operation needs, such as an image registry, can't be reached.
    #[error("{0}")]
    Unavailable(String),
    /// microsandbox or the secret store failed.
    #[error("{0}")]
    Internal(String),
}

impl SandboxError {
    fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    fn failed_precondition(message: impl Into<String>) -> Self {
        Self::FailedPrecondition(message.into())
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

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
    /// [`mise::DOTFILES_PATH`] when the sandbox is created.
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

/// A change that recreates a sandbox: new egress rules on its own disk, or a new image.
#[derive(Debug, Clone, Copy)]
enum Change<'a> {
    /// New egress rules. The sandbox comes back from the snapshot of its disk.
    Network,
    /// A new image. The sandbox gets a fresh root filesystem and Docker disk from the image.
    Image(&'a str),
}

impl Change<'_> {
    /// Returns what the change does to the sandbox, for messages such as "failed to {what}".
    fn what(self, name: &str) -> String {
        match self {
            Change::Network => format!("update the network rules of {name}"),
            Change::Image(_) => format!("upgrade sandbox {name}"),
        }
    }

    /// Returns what the sandbox keeps when the change fails before it's removed.
    fn kept(self) -> &'static str {
        match self {
            Change::Network => "it keeps its old rules",
            Change::Image(_) => "it keeps its old image",
        }
    }

    /// Returns the word that names the snapshots of the change.
    fn snapshot_kind(self) -> &'static str {
        match self {
            Change::Network => "network",
            Change::Image(_) => "upgrade",
        }
    }

    /// Returns what the change did, for messages about failures after it's done.
    fn done(self, name: &str) -> String {
        match self {
            Change::Network => format!("updated the network rules of {name}"),
            Change::Image(_) => format!("upgraded sandbox {name}"),
        }
    }
}

/// How to recreate a sandbox: with its settings and egress rules, and the change.
#[derive(Debug, Clone, Copy)]
struct Recreation<'a> {
    settings: &'a SandboxSettings,
    network: &'a NetworkSpec,
    change: Change<'a>,
}

/// Returns the guest path a sandbox mounts its workspace at, which is its working directory.
fn workspace_path_of(handle: &SandboxHandle) -> Option<String> {
    handle.config().ok()?.spec.runtime.workdir
}

/// Returns the host directory a sandbox bind-mounts at its workspace path. microsandbox stores
/// the host path canonicalized when it creates the sandbox.
fn workspace_host_path_of(handle: &SandboxHandle) -> Option<String> {
    workspace_host_path_in(&handle.config().ok()?.spec)
}

/// Returns the host directory the sandbox config bind-mounts at its workspace path.
fn workspace_host_path_in(spec: &SandboxSpec) -> Option<String> {
    let workdir = spec.runtime.workdir.as_deref()?;

    spec.mounts.iter().find_map(|mount| match mount {
        VolumeMount::Bind { host, guest, .. } if guest == workdir => {
            Some(host.to_string_lossy().into_owned())
        }
        _ => None,
    })
}

/// The settings a sandbox is created with, apart from its root filesystem and egress rules.
/// Recreating a sandbox reads them from the sandbox, so it keeps them.
#[derive(Debug, Clone)]
struct SandboxSettings {
    name: String,
    /// Labels such as the SSH host name and the mise setting.
    labels: BTreeMap<String, String>,
    /// The host directory mounted as the workspace and its guest path, which is the workdir.
    workspace: Option<Workspace>,
    /// Extra host directories bind-mounted next to the workspace.
    mounts: Vec<MountSpec>,
    /// Environment variables from the spec, which [`env::NAMES_LABEL`] names.
    env: BTreeMap<String, String>,
    init: bool,
    resources: Resources,
}

/// A host directory bind-mounted into the sandbox as its working directory.
#[derive(Debug, Clone)]
struct Workspace {
    host: String,
    guest: String,
}

impl SandboxSettings {
    /// Reads the settings of an existing sandbox from the config microsandbox stores for it.
    fn of(handle: &SandboxHandle) -> Result<Self, MicrosandboxError> {
        let spec = handle.config()?.spec;
        let workspace = workspace_host_path_of(handle)
            .zip(workspace_path_of(handle))
            .map(|(host, guest)| Workspace { host, guest });
        let workdir = spec.runtime.workdir.as_deref();

        Ok(Self {
            name: handle.name().to_string(),
            mounts: extra_mounts(&spec.mounts, workdir),
            env: env::stored(&spec),
            labels: spec.labels,
            workspace,
            init: spec.init.is_some(),
            resources: Resources {
                cpus: spec.resources.cpus,
                memory_mib: spec.resources.memory_mib,
                docker_volume_mib: docker_volume_mib(&spec.mounts),
            },
        })
    }

    /// Returns a builder for a detached sandbox with these settings, the route of its network
    /// log in `netlogs` and, when it shares the host clipboard, the route of its clipboard in
    /// `clipboards`, without a root filesystem.
    fn builder(&self, netlogs: &NetLogs, clipboards: &Clipboards) -> SandboxBuilder {
        let builder = self
            .labels
            .iter()
            .fold(Sandbox::builder(&self.name), |builder, (key, value)| {
                builder.label(key, value)
            })
            .detached(true);
        let builder = with_workspace(builder, self.workspace.as_ref());
        let builder = with_mounts(builder, &self.mounts);
        let builder = env::with_env(builder, &self.env);
        let builder = ssh::with_login_shell(builder);
        let builder = netlogs.add_route(builder, &self.name);
        let builder = if clipboard::is_enabled(&self.labels) {
            clipboards.add_route(builder, &self.name)
        } else {
            builder
        };

        with_init(with_resources(builder, self.resources), self.init)
    }
}

/// Returns the extra host directories among the sandbox's mounts: the bind mounts other than
/// the workspace at `workdir`.
fn extra_mounts(mounts: &[VolumeMount], workdir: Option<&str>) -> Vec<MountSpec> {
    mounts
        .iter()
        .filter_map(|mount| match mount {
            VolumeMount::Bind {
                host,
                guest,
                options,
                ..
            } if Some(guest.as_str()) != workdir => Some(MountSpec {
                host: host.to_string_lossy().into_owned(),
                guest: guest.clone(),
                readonly: options.readonly,
            }),
            _ => None,
        })
        .collect()
}

/// Returns the size of the sandbox's Docker disk, or the default size when it has none.
fn docker_volume_mib(mounts: &[VolumeMount]) -> u32 {
    mounts
        .iter()
        .find_map(|mount| match mount {
            VolumeMount::Owned {
                guest,
                storage: OwnedVolumeStorage::Disk { capacity_mib },
                ..
            } if guest == DOCKER_DATA_PATH => Some(*capacity_mib),
            _ => None,
        })
        .unwrap_or_else(default_docker_volume_mib)
}

/// Returns the default size of the Docker disk in MiB.
fn default_docker_volume_mib() -> u32 {
    // The default size is a constant that parses; 20 GiB matches it should that ever change.
    firebrick_spec::parse_size_mib(&VolumesSpec::default().docker).unwrap_or(20 * 1024)
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

    /// Waits up to `timeout` for the updates and upgrades of sandboxes that are running.
    /// Returns whether they finished. Used when the daemon shuts down, after it stopped taking
    /// requests.
    pub async fn finish_recreations(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.recreations.write())
            .await
            .is_ok()
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

    /// Fails with `InvalidArgument` when a requested environment variable has the name of a
    /// secret the sandbox gets, which would hide the secret's placeholder.
    fn check_env_against_secrets(&self, request: StartSandbox<'_>) -> Result<(), SandboxError> {
        let Some(env) = request.env.filter(|env| !env.is_empty()) else {
            return Ok(());
        };
        let secrets = self.load_secrets_for(request.name)?;

        match secrets::conflicting_name(&secrets, env.keys()) {
            Some(name) => Err(SandboxError::invalid_argument(format!(
                "env variable {name} conflicts with secret {name}; remove one of them"
            ))),
            None => Ok(()),
        }
    }

    /// Opens the forwards of the sandboxes that are running, from their stored ports. Used
    /// when the daemon starts.
    pub async fn restore_forwards(&self) {
        let sandboxes = match list_all_sandboxes().await {
            Ok(sandboxes) => sandboxes,
            Err(err) => {
                tracing::warn!("failed to restore port forwards: {err}");
                return;
            }
        };
        let _ports = self.ports_lock.lock().await;

        for sb in sandboxes
            .iter()
            .filter(|sb| sb.status_snapshot() == SandboxStatus::Running)
        {
            self.forwards.apply(sb.name(), &stored_ports(sb)).await;
        }
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

    /// Stores the ports with the sandbox when they're given, then makes its open forwards
    /// match the stored ports.
    async fn open_forwards(&self, name: &str, ports: Option<&[PortMapping]>) -> ForwardReport {
        let _ports = self.ports_lock.lock().await;
        let ports = match (ports, Sandbox::get(name).await) {
            (Some(ports), Ok(sb)) => {
                store_ports(&sb, ports).await;
                ports.to_vec()
            }
            (Some(ports), Err(_)) => ports.to_vec(),
            (None, Ok(sb)) => stored_ports(&sb),
            (None, Err(_)) => vec![],
        };

        self.forwards.apply(name, &ports).await
    }

    /// Adds the forward to the ports stored with the sandbox, replacing a stored one with the
    /// same host port, and opens it when the sandbox runs. Returns whether the sandbox runs.
    /// Fails with `FailedPrecondition` when the host port can't be listened on, and then keeps
    /// the stored ports and the open forwards.
    pub async fn forward_port(&self, name: &str, port: PortMapping) -> Result<bool, SandboxError> {
        let _guard = self.lock_sandbox(name).await;
        let _ports = self.ports_lock.lock().await;
        let sb = get_sandbox(name).await?;
        let stored = stored_ports(&sb);
        let change = PortsChange {
            ports: with_port(&stored, port),
            stored,
            requested: Some(port),
        };

        self.change_ports(&sb, &change).await
    }

    /// Removes the forward of the host port from the ports stored with the sandbox, and closes
    /// it when the sandbox runs. Returns whether the sandbox runs. Fails with `NotFound` when
    /// the sandbox doesn't forward the host port.
    pub async fn remove_port(&self, name: &str, host: u16) -> Result<bool, SandboxError> {
        let _guard = self.lock_sandbox(name).await;
        let _ports = self.ports_lock.lock().await;
        let sb = get_sandbox(name).await?;
        let stored = stored_ports(&sb);
        let ports: Vec<PortMapping> = stored.iter().filter(|p| p.host != host).copied().collect();

        if ports.len() == stored.len() {
            return Err(SandboxError::not_found(format!(
                "port {host} isn't forwarded for sandbox {name}"
            )));
        }

        let change = PortsChange {
            stored,
            ports,
            requested: None,
        };

        self.change_ports(&sb, &change).await
    }

    /// Makes the open forwards of a running sandbox match the changed ports, then stores them
    /// with the sandbox. Returns whether the sandbox runs. When a new forward can't be opened or
    /// the ports can't be stored, reopens the stored forwards and fails.
    async fn change_ports(
        &self,
        sb: &SandboxHandle,
        change: &PortsChange,
    ) -> Result<bool, SandboxError> {
        let running = sb.status_snapshot() == SandboxStatus::Running;

        if running {
            self.apply_new_ports(sb.name(), change).await?;
        }

        let saved = save_ports(sb, &change.ports).await;

        if saved.is_err() && running {
            self.forwards.apply(sb.name(), &change.stored).await;
        }

        saved.map(|()| running)
    }

    /// Makes the open forwards of the sandbox match the changed ports. Fails with
    /// `FailedPrecondition` when a new forward can't be opened, after reopening the stored
    /// forwards.
    async fn apply_new_ports(&self, name: &str, change: &PortsChange) -> Result<(), SandboxError> {
        let report = self.forwards.apply(name, &change.ports).await;
        let Some(failure) = report.failed.iter().find(|f| change.is_new(f.port)) else {
            return Ok(());
        };

        self.forwards.apply(name, &change.stored).await;

        Err(SandboxError::failed_precondition(failure.reason.clone()))
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

    /// Removes the sandbox-scoped secrets of a removed sandbox from the store, so a later
    /// sandbox with the same name doesn't inherit them.
    async fn remove_sandbox_secrets(&self, name: &str) -> Result<(), SandboxError> {
        let _secrets_guard = self.secrets_lock.lock().await;

        self.secrets.remove_sandbox(name).map_err(|err| {
            tracing::error!(error = ?err, "failed to remove the secrets of sandbox {name}");
            SandboxError::internal(format!(
                "removed sandbox {name}, but failed to remove its secrets"
            ))
        })?;

        Ok(())
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

    /// Replaces the egress rules of an existing sandbox by recreating it from a disk snapshot,
    /// so it keeps its root disk, Docker disk and settings but not its processes, and gets the
    /// route of its network log, also when it had none. The sandbox ends in the state it was
    /// in. Returns whether it was recreated: a sandbox that already has the rules is left alone. A paused sandbox is refused, because it can't be paused
    /// again. When the recreate fails, the snapshot is kept so the user can recover the
    /// sandbox, and the error names it.
    pub async fn update_network(
        &self,
        name: &str,
        network: &NetworkSpec,
    ) -> Result<bool, SandboxError> {
        let _recreation = self.recreations.read().await;
        let _guard = self.lock_sandbox(name).await;
        let sb = get_sandbox(name).await?;
        let settings =
            SandboxSettings::of(&sb).map_err(|err| change_failed(name, Change::Network, &err))?;

        if settings.labels.get(network::RULES_LABEL) == Some(&network::rules_label(network)) {
            return Ok(false);
        }

        if current_status(&sb).await? == SandboxStatus::Paused {
            return Err(SandboxError::failed_precondition(format!(
                "sandbox {name} is paused; resume it before updating its network rules"
            )));
        }

        let recreation = Recreation {
            settings: &settings,
            network,
            change: Change::Network,
        };
        self.recreate_and_reopen(&sb, &recreation).await?;

        Ok(true)
    }

    /// Recreates an existing sandbox from the latest version of `image`, or the default image
    /// when it's empty, passing the progress of the image pull to `on_pull`. The image is
    /// pulled first, so a failed pull leaves the sandbox alone. The sandbox keeps its settings,
    /// secrets, egress rules and ports, but gets a fresh root filesystem and Docker disk, and
    /// ends in the state it was in. A sandbox whose image already has the pulled digest is left
    /// alone. A paused sandbox is refused. When the create fails, the snapshot of the old disk
    /// is kept so the user can recover the sandbox, and the error names it.
    pub async fn upgrade(
        &self,
        name: &str,
        image: &str,
        on_pull: impl FnMut(PullUpdate) + Send,
    ) -> Result<Upgrade, SandboxError> {
        let _recreation = self.recreations.read().await;
        let image = sandbox_image(image);
        refuse_paused(&get_sandbox(name).await?).await?;

        // The pull can take minutes, so it runs before the sandbox is locked.
        let digest = pull_image(name, image, on_pull).await?;

        let _guard = self.lock_sandbox(name).await;
        // Get the sandbox again, because it may have changed while the image was pulled.
        let sb = get_sandbox(name).await?;
        refuse_paused(&sb).await?;
        let config = sb
            .config()
            .map_err(|err| change_failed(name, Change::Image(image), &err))?;
        let network = network::rules_of(&config.spec);
        let old_image = image_reference(config.spec.image).ok_or_else(|| {
            SandboxError::failed_precondition(format!(
                "sandbox {name} wasn't created from an image, so it can't be upgraded"
            ))
        })?;
        let upgraded = current_digest(&sb)? != digest;

        if upgraded {
            self.recreate_from_image(&sb, &network, image).await?;
        }

        Ok(Upgrade {
            upgraded,
            old_image,
            new_image: image.to_string(),
        })
    }

    /// Recreates the sandbox from the pulled image with the egress rules.
    async fn recreate_from_image(
        &self,
        sb: &SandboxHandle,
        network: &NetworkSpec,
        image: &str,
    ) -> Result<(), SandboxError> {
        let change = Change::Image(image);
        let mut settings =
            SandboxSettings::of(sb).map_err(|err| change_failed(sb.name(), change, &err))?;
        settings
            .labels
            .insert(IMAGE_LABEL.to_string(), image.to_string());
        let recreation = Recreation {
            settings: &settings,
            network,
            change,
        };

        self.recreate_and_reopen(sb, &recreation).await
    }

    /// Removes the upgrade probes that an earlier daemon left behind, for example because it
    /// was killed during a pull, logging a warning when they can't be listed. Used when the
    /// daemon starts, so only the probes of other processes are removed.
    pub async fn remove_stale_probes(&self) {
        let sandboxes = match list_all_sandboxes().await {
            Ok(sandboxes) => sandboxes,
            Err(err) => {
                tracing::warn!("failed to remove stale upgrade probes: {err}");
                return;
            }
        };
        let own = format!("{PROBE_PREFIX}{}-", std::process::id());

        for sb in sandboxes
            .iter()
            .filter(|sb| is_probe(sb) && !sb.name().starts_with(&own))
        {
            remove_leftover(sb.name(), "stale upgrade probe").await;
        }
    }

    /// Listens for the sandbox's network log, and its clipboard when it shares the host
    /// clipboard, replaces the sandbox as the recreation says, then syncs the SSH config and the
    /// editor settings and reopens its forwards and URL relay when it runs, also when replacing
    /// it failed.
    async fn recreate_and_reopen(
        &self,
        sb: &SandboxHandle,
        recreation: &Recreation<'_>,
    ) -> Result<(), SandboxError> {
        let name = sb.name();

        self.netlogs.listen(name).await;
        if clipboard::is_enabled(&recreation.settings.labels) {
            self.listen_for_clipboard(name, &recreation.settings.labels)
                .await;
        }
        let result = self.replace(sb, recreation).await;
        sync_ssh_config().await;
        self.reopen_forwards_and_relay(name).await;

        result
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

    /// Stops the sandbox when it's live, takes a snapshot of its disk and replaces it with one
    /// created with the change and the egress rules, and stops that one again when the sandbox
    /// was stopped. A new image gets the workspace's mise tools when the sandbox has mise
    /// enabled; a failed install is returned once the sandbox is in the state it was in.
    async fn replace(
        &self,
        sb: &SandboxHandle,
        recreation: &Recreation<'_>,
    ) -> Result<(), SandboxError> {
        let name = sb.name();
        let change = recreation.change;
        let was_live = is_live(sb.status_snapshot());

        if was_live {
            stop_or_kill(sb, STOP_TIMEOUT)
                .await
                .map_err(|err| change_failed(name, change, &err))?;
            self.forwards.close(name).await;
            self.relays.forget(name);
        }

        let snapshot = match snapshot_and_remove(sb, change).await {
            Ok(snapshot) => snapshot,
            Err(err) => return Err(snapshot_failed(sb, was_live, change, &err).await),
        };

        let recreated = self.recreate(recreation, &snapshot).await?;
        let installed = install_tools_after(change, &recreated, recreation.settings).await;

        // The recreated sandbox boots, so stop it again when it was stopped before.
        if !was_live {
            stop_recreated(name, change).await?;
        }

        installed
    }

    /// Creates the removed sandbox again with the change and the egress rules, then deletes
    /// the snapshot of its disk.
    async fn recreate(
        &self,
        recreation: &Recreation<'_>,
        snapshot: &str,
    ) -> Result<Sandbox, SandboxError> {
        let Recreation {
            settings,
            network,
            change,
        } = *recreation;
        let name = &settings.name;
        let builder = match change {
            // `image(..)` would discard the pending snapshot, so the builder must not set one.
            Change::Network => settings
                .builder(&self.netlogs, &self.clipboards)
                .override_snapshot(snapshot),
            Change::Image(image) => settings
                .builder(&self.netlogs, &self.clipboards)
                .image(image),
        };

        let sb = match self.create_from(settings, network, builder).await {
            Ok(sb) => sb,
            Err(err) => return Err(recreate_failed(name, snapshot, change, &err).await),
        };

        remove_snapshot(snapshot).await;
        tracing::info!("{}", change.done(name));

        Ok(sb)
    }

    /// Creates the sandbox of the settings from the builder with its secrets and the egress
    /// rules. microsandbox boots it.
    async fn create_from(
        &self,
        settings: &SandboxSettings,
        network: &NetworkSpec,
        builder: SandboxBuilder,
    ) -> Result<Sandbox, String> {
        let _secrets_guard = self.secrets_lock.lock().await;
        let secrets = self
            .load_secrets_for(&settings.name)
            .map_err(|err| err.to_string())?;

        // A snapshot has no image to pull, and a new image was pulled before, so there's no
        // progress to report.
        create_with(builder, &secrets, network, |_| {})
            .await
            .map_err(|err| format!("{err:?}"))
    }

    /// Stores a secret and adds it to the sandboxes in its scope: the sandbox it belongs to, or,
    /// for a global secret, every existing sandbox without a sandbox-scoped secret with that
    /// name. Running sandboxes pick it up the next time they start. Returns the names of the
    /// sandboxes it couldn't be added to.
    pub async fn set_secret(&self, request: NewSecret) -> Result<Vec<String>, SandboxError> {
        let secret = Secret::new(request.name, request.value, request.allowed_hosts)
            .map_err(|err| SandboxError::invalid_argument(err.to_string()))?
            .in_scope(request.sandbox);

        let _secrets_guard = self.secrets_lock.lock().await;
        let scoped_handle = scoped_sandbox(secret.sandbox()).await?;
        self.check_secret_against_env(scoped_handle.as_ref(), secret.name())
            .await?;

        self.secrets.set(secret.clone()).map_err(|err| {
            tracing::error!(error = ?err, "failed to store secret {}", secret.name());
            SandboxError::internal("failed to store secret")
        })?;

        let change = SecretChange::Add(&secret);
        let failed_sandboxes = self
            .update_scope(scoped_handle.as_ref(), secret.name(), change)
            .await
            .map_err(|_| {
                SandboxError::internal(
                    "stored the secret, but failed to list the sandboxes to add it to",
                )
            })?;

        tracing::info!(
            "set secret {}",
            describe_secret(secret.name(), secret.sandbox())
        );

        Ok(failed_sandboxes)
    }

    /// Returns the stored secrets, sorted by name, then by scope with the global secret first.
    pub fn list_secrets(&self) -> Result<Vec<Secret>, SandboxError> {
        let mut secrets = self.load_secrets()?;
        secrets.sort_by(|a, b| (a.name(), a.sandbox()).cmp(&(b.name(), b.sandbox())));
        Ok(secrets)
    }

    /// Removes a stored secret and removes it from the sandboxes in its scope, like
    /// [`SandboxManager::set_secret`]. A sandbox that loses its sandbox-scoped secret gets the
    /// global secret with the same name back, if there is one. Running sandboxes keep the old
    /// secret until they restart. Returns the names of the sandboxes the change failed for, in
    /// which case the secret stays in the store so the removal can be retried.
    pub async fn remove_secret(
        &self,
        name: &str,
        sandbox: Option<&str>,
    ) -> Result<Vec<String>, SandboxError> {
        secrets::validate_name(name)
            .map_err(|err| SandboxError::invalid_argument(err.to_string()))?;

        let _secrets_guard = self.secrets_lock.lock().await;
        let scoped_handle = scoped_sandbox(sandbox).await?;
        let stored = self.load_secrets()?;
        ensure_secret_exists(&stored, name, sandbox)?;

        // Remove the secret from the store last, so it can be removed again when a sandbox
        // fails.
        let change = removal(&stored, name, sandbox);
        let failed_sandboxes = self
            .update_scope(scoped_handle.as_ref(), name, change)
            .await?;

        if !failed_sandboxes.is_empty() {
            return Ok(failed_sandboxes);
        }

        self.secrets.remove(name, sandbox).map_err(|err| {
            tracing::error!(error = ?err, "failed to remove secret {name}");
            SandboxError::internal("failed to remove secret")
        })?;

        tracing::info!("removed secret {}", describe_secret(name, sandbox));

        Ok(failed_sandboxes)
    }

    /// Applies a change to the secret with the name to the sandbox it belongs to, or, without
    /// one, to the sandboxes that get the global secret. Returns the names of the sandboxes the
    /// change failed for.
    async fn update_scope(
        &self,
        scoped_handle: Option<&SandboxHandle>,
        name: &str,
        change: SecretChange<'_>,
    ) -> Result<Vec<String>, SandboxError> {
        match scoped_handle {
            Some(handle) => Ok(apply_to_sandbox(handle, &change).await),
            None => self.update_unscoped_sandboxes(name, change).await,
        }
    }

    /// Fails with `FailedPrecondition` when a sandbox the secret goes to has a variable from its
    /// spec with the secret's name, which would hide the secret's placeholder: the scoped
    /// sandbox, or every firebrick sandbox without its own secret with the name.
    async fn check_secret_against_env(
        &self,
        scoped_handle: Option<&SandboxHandle>,
        name: &str,
    ) -> Result<(), SandboxError> {
        let conflicting = match scoped_handle {
            Some(handle) => sets_variable(handle, name).then(|| handle.name().to_string()),
            None => self
                .unscoped_sandboxes(name)
                .await?
                .iter()
                .find(|handle| sets_variable(handle, name))
                .map(|handle| handle.name().to_string()),
        };

        match conflicting {
            Some(sandbox) => Err(SandboxError::failed_precondition(format!(
                "secret {name} conflicts with env variable {name} of sandbox {sandbox}; remove \
                 one of them"
            ))),
            None => Ok(()),
        }
    }

    /// Returns the firebrick sandboxes a global secret with the name goes to: the ones without
    /// a sandbox-scoped secret with that name.
    async fn unscoped_sandboxes(&self, name: &str) -> Result<Vec<SandboxHandle>, SandboxError> {
        let overridden = self.overriding_sandboxes(name)?;

        Ok(list_all_sandboxes()
            .await?
            .into_iter()
            .filter(|handle| ssh::hostname_of(handle).is_some())
            .filter(|handle| !overridden.contains(handle.name()))
            .collect())
    }

    /// Returns the names of the sandboxes with a sandbox-scoped secret with the name.
    fn overriding_sandboxes(&self, name: &str) -> Result<HashSet<String>, SandboxError> {
        Ok(self
            .load_secrets()?
            .iter()
            .filter(|secret| secret.name() == name)
            .filter_map(|secret| secret.sandbox().map(str::to_string))
            .collect())
    }

    /// Applies a change to a global secret to every firebrick sandbox, except the ones with a
    /// sandbox-scoped secret with the same name. Returns the names of the sandboxes the change
    /// failed for.
    async fn update_unscoped_sandboxes(
        &self,
        name: &str,
        change: SecretChange<'_>,
    ) -> Result<Vec<String>, SandboxError> {
        let overridden = self.overriding_sandboxes(name)?;

        update_firebrick_sandboxes(change, &overridden).await
    }

    /// Returns the stored secrets the sandbox with the name gets when it's created: the global
    /// secrets, with its own sandbox-scoped secrets in place of the global ones with the same
    /// name. A new sandbox only has scoped secrets left over from an earlier sandbox with its
    /// name, for example one removed without fbkd, which global changes skip.
    fn load_secrets_for(&self, sandbox: &str) -> Result<Vec<Secret>, SandboxError> {
        Ok(secrets::for_sandbox(self.load_secrets()?, sandbox))
    }

    /// Returns the stored secrets.
    fn load_secrets(&self) -> Result<Vec<Secret>, SandboxError> {
        self.secrets.load().map_err(|err| {
            tracing::error!(error = ?err, "failed to load secrets");
            SandboxError::internal("failed to load secrets")
        })
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

/// Returns the settings of a new sandbox for the request, with its workspace mounted at
/// `guest_path` and the SSH host name.
fn new_sandbox_settings(
    request: StartSandbox<'_>,
    guest_path: &str,
    hostname: &str,
    resources: Resources,
) -> SandboxSettings {
    SandboxSettings {
        name: request.name.to_string(),
        labels: new_sandbox_labels(request, hostname),
        workspace: Some(Workspace {
            host: request.workspace.to_string(),
            guest: guest_path.to_string(),
        }),
        mounts: request.mounts.to_vec(),
        env: request.env.cloned().unwrap_or_default(),
        init: request.init,
        resources,
    }
}

/// Returns the labels of a new sandbox for the request with the SSH host name. Only a sandbox
/// that shares the host clipboard gets the clipboard labels.
fn new_sandbox_labels(request: StartSandbox<'_>, hostname: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        (ssh::HOSTNAME_LABEL.to_string(), hostname.to_string()),
        (
            mise::ENABLED_LABEL.to_string(),
            mise::label_value(request.mise).to_string(),
        ),
        (
            forward::PORTS_LABEL.to_string(),
            forward::label_value(request.ports.unwrap_or_default()),
        ),
        (
            IMAGE_LABEL.to_string(),
            sandbox_image(request.image).to_string(),
        ),
    ]);

    if request.clipboard {
        labels.insert(clipboard::ENABLED_LABEL.to_string(), "true".to_string());
        labels.insert(clipboard::COPY_LABEL.to_string(), "true".to_string());
    }

    labels
}

/// Mounts the workspace read/write and makes it the working directory. Mirror propagates guest
/// chmod changes to the host files, and the agent user (1000:1000) owns the mount in the guest.
fn with_workspace(builder: SandboxBuilder, workspace: Option<&Workspace>) -> SandboxBuilder {
    let Some(workspace) = workspace else {
        return builder;
    };

    builder
        .volume(&workspace.guest, |m| {
            m.bind(&workspace.host)
                .host_permissions(HostPermissions::Mirror)
                .owner(1000, 1000)
        })
        .workdir(&workspace.guest)
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

/// Takes a disk snapshot of the stopped sandbox, then removes the sandbox. Returns the path of
/// the snapshot. When the sandbox can't be removed, the snapshot is deleted again.
async fn snapshot_and_remove(
    sb: &SandboxHandle,
    change: Change<'_>,
) -> Result<String, MicrosandboxError> {
    let name = sb.name();
    let snapshot = Snapshot::builder(snapshot_name(name, change))
        .from_sandbox(name)
        .create()
        .await?;
    // microsandbox files the snapshot under a group named after the sandbox, where its bare
    // name doesn't select it, so the path refers to it.
    let path = snapshot.path()?.to_string_lossy().into_owned();

    if let Err(err) = sb.remove().await {
        remove_snapshot(&path).await;
        return Err(err);
    }

    Ok(path)
}

/// Logs why the sandbox couldn't be snapshotted and removed, starts it again when it was live
/// so it ends in the state it was in, and returns the error the client sees.
async fn snapshot_failed(
    sb: &SandboxHandle,
    was_live: bool,
    change: Change<'_>,
    err: &MicrosandboxError,
) -> SandboxError {
    let name = sb.name();
    let message = format!("failed to {}; {}", change.what(name), change.kept());
    tracing::error!(error = ?err, "failed to snapshot and remove sandbox {name}");

    if !was_live {
        return SandboxError::internal(message);
    }

    match sb.start_detached().await {
        Ok(_) => SandboxError::internal(message),
        Err(err) => {
            tracing::error!(error = ?err, "failed to start sandbox {name} again");
            SandboxError::internal(format!("{message}, but it couldn't be started again"))
        }
    }
}

/// Stops a sandbox that was recreated while it was stopped. By then the change is done, so the
/// error says so.
async fn stop_recreated(name: &str, change: Change<'_>) -> Result<(), SandboxError> {
    let result = match Sandbox::get(name).await {
        Ok(sb) => stop_or_kill(&sb, STOP_TIMEOUT).await,
        Err(err) => Err(err),
    };

    result.map_err(|err| {
        tracing::error!(error = ?err, "failed to stop sandbox {name} after recreating it");
        SandboxError::internal(format!(
            "{}, but failed to stop it again",
            change.done(name)
        ))
    })
}

/// Trusts and installs the workspace's mise tools in a sandbox that got a new image, because
/// its fresh root filesystem doesn't have them, when the sandbox has mise enabled. A sandbox
/// recreated from its own disk keeps them.
async fn install_tools_after(
    change: Change<'_>,
    sb: &Sandbox,
    settings: &SandboxSettings,
) -> Result<(), SandboxError> {
    match (change, &settings.workspace) {
        (Change::Image(_), Some(workspace)) if mise::is_enabled(&settings.labels) => {
            // The upgrade stream only reports the image pull, so the install's output isn't
            // passed on.
            install_mise_tools(sb, &workspace.guest, |_| {}).await
        }
        _ => Ok(()),
    }
}

/// Returns a snapshot name for the change of the sandbox that no other change uses.
fn snapshot_name(sandbox: &str, change: Change<'_>) -> String {
    format!(
        "firebrick-{}-{sandbox}-{}",
        change.snapshot_kind(),
        unique_suffix()
    )
}

/// Deletes the snapshot, logging a warning when that fails. A leftover snapshot only takes up
/// disk space, so it doesn't fail the update.
///
/// microsandbox refuses to delete a snapshot while another operation reads it, such as a
/// concurrent snapshot listing, and doesn't wait for that reader. So the delete is tried again
/// until [`SNAPSHOT_IN_USE_TIMEOUT`] has passed.
async fn remove_snapshot(snapshot: &str) {
    let deadline = Instant::now() + SNAPSHOT_IN_USE_TIMEOUT;

    loop {
        match Snapshot::remove(snapshot, false).await {
            Ok(()) => return,
            Err(err) if is_in_use(&err) && Instant::now() < deadline => {
                sleep(SNAPSHOT_RETRY_INTERVAL).await;
            }
            Err(err) => {
                tracing::warn!(error = ?err, "failed to remove snapshot {snapshot}");
                return;
            }
        }
    }
}

/// Returns whether microsandbox refused to delete a snapshot because another operation uses it.
/// microsandbox reports that only as a `Custom` error with this message.
fn is_in_use(err: &MicrosandboxError) -> bool {
    matches!(err, MicrosandboxError::Custom(message) if message.starts_with("snapshot is in use"))
}

/// Logs why the change failed and returns the error the client sees.
fn change_failed(name: &str, change: Change<'_>, err: &MicrosandboxError) -> SandboxError {
    let message = format!("failed to {}", change.what(name));
    tracing::error!(error = ?err, "{message}");
    SandboxError::internal(message)
}

/// Removes what a failed recreate left of the sandbox, logs the error and the snapshot that
/// still holds the sandbox's disk, and returns the error the client sees.
async fn recreate_failed(
    name: &str,
    snapshot: &str,
    change: Change<'_>,
    err: &str,
) -> SandboxError {
    remove_leftover(name, "sandbox after a failed recreate").await;

    tracing::error!(
        error = err,
        "failed to recreate sandbox {name}; its disk is kept as snapshot {snapshot}"
    );

    SandboxError::internal(format!(
        "failed to {}; the sandbox was kept as snapshot {snapshot}",
        change.what(name)
    ))
}

/// Label of an upgrade probe sandbox. Its value is the name of the sandbox it pulls the image
/// for.
pub const PROBE_LABEL: &str = "firebrick.upgrade-probe";

/// Start of the names of upgrade probe sandboxes, followed by the daemon's process id.
const PROBE_PREFIX: &str = "fbk-upgrade-";

/// Whether the sandbox is an upgrade probe.
fn is_probe(sb: &SandboxHandle) -> bool {
    sb.config()
        .is_ok_and(|config| config.spec.labels.contains_key(PROBE_LABEL))
}

/// Returns a suffix for a name that no other name of this process uses: the process id and
/// the current time in nanoseconds.
fn unique_suffix() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());

    format!("{}-{nonce}", std::process::id())
}

/// Pulls the latest version of the image for the sandbox, passing the progress to `on_pull`,
/// and returns its manifest digest. microsandbox only pulls images while it creates a sandbox,
/// so this creates a short-lived probe sandbox without network from the image, labeled with
/// the sandbox's name, reads the digest it was created from and removes it again. Fails like
/// a create of a sandbox from the image.
async fn pull_image(
    sandbox: &str,
    image: &str,
    on_pull: impl FnMut(PullUpdate) + Send,
) -> Result<String, SandboxError> {
    let name = format!("{PROBE_PREFIX}{}", unique_suffix());
    let builder = Sandbox::builder(&name)
        .image(image)
        .label(PROBE_LABEL, sandbox)
        // A tag such as `node:22` can move, so ask the registry for its manifest.
        .pull_policy(PullPolicy::Always)
        .disable_network()
        .detached(true);
    let created = match builder.create_detached_with_pull_progress() {
        Ok((progress, task)) => pull::wait_for_create(progress, task, on_pull).await,
        Err(err) => Err(err),
    };
    let digest = match created {
        Ok(_) => probe_digest(&name).await.map_err(|err| {
            tracing::error!("failed to read the digest of image {image}: {err}");
            SandboxError::internal(format!("failed to pull image {image}"))
        }),
        Err(err) => {
            tracing::error!(error = ?err, "failed to pull image {image}");
            Err(create_error(image, &err))
        }
    };

    remove_leftover(&name, "upgrade probe").await;

    digest
}

/// Returns the manifest digest the probe sandbox was created from, or why it can't be read.
async fn probe_digest(name: &str) -> Result<String, String> {
    let sb = Sandbox::get(name)
        .await
        .map_err(|err| format!("failed to get probe sandbox {name}: {err}"))?;

    manifest_digest(&sb)
}

/// Returns the manifest digest of the image the sandbox was created from. Fails with
/// `Internal` when it can't be read, so a sandbox whose image is unknown isn't recreated.
fn current_digest(sb: &SandboxHandle) -> Result<String, SandboxError> {
    manifest_digest(sb).map_err(|err| {
        tracing::error!(
            "failed to read the image digest of sandbox {}: {err}",
            sb.name()
        );
        SandboxError::internal(format!(
            "failed to read the image digest of sandbox {}",
            sb.name()
        ))
    })
}

/// Returns the manifest digest of the image the sandbox was created from, or why it can't be
/// read. microsandbox keeps it in the sandbox's config, but doesn't expose it, so it's read
/// from the config's JSON.
fn manifest_digest(sb: &SandboxHandle) -> Result<String, String> {
    let config: serde_json::Value = serde_json::from_str(sb.config_json())
        .map_err(|err| format!("the config isn't valid JSON: {err}"))?;

    config
        .get("manifest_digest")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the config has no manifest_digest".to_string())
}

/// Stops and removes the sandbox when it exists, logging a warning that names it as `what`
/// when it can't be removed.
async fn remove_leftover(name: &str, what: &str) {
    let Ok(sb) = Sandbox::get(name).await else {
        return;
    };
    let _ = stop_or_kill(&sb, STOP_TIMEOUT).await;

    if let Err(err) = sb.remove().await {
        tracing::warn!(error = ?err, "failed to remove {what} {name}");
    }
}

/// Fails with `FailedPrecondition` when the sandbox is paused, because an upgrade can't pause
/// it again.
async fn refuse_paused(sb: &SandboxHandle) -> Result<(), SandboxError> {
    if current_status(sb).await? != SandboxStatus::Paused {
        return Ok(());
    }

    Err(SandboxError::failed_precondition(format!(
        "sandbox {} is paused; resume or stop it before upgrading",
        sb.name()
    )))
}

/// Returns the image reference of an OCI root filesystem, or `None` for another one.
fn image_reference(image: RootfsSource) -> Option<String> {
    match image {
        RootfsSource::Oci(oci) => Some(oci.reference),
        _ => None,
    }
}

/// Adds the egress rules, the label that records them and the secrets to the builder and
/// creates the detached sandbox, passing the progress of its image pull to `on_pull`. The
/// rules go first, because they replace the TLS settings that the secrets turn on.
async fn create_with(
    builder: SandboxBuilder,
    secrets: &[Secret],
    network: &NetworkSpec,
    on_pull: impl FnMut(PullUpdate) + Send,
) -> Result<Sandbox, MicrosandboxError> {
    let builder = network::add_to_builder(builder, network)?
        .label(network::RULES_LABEL, network::rules_label(network));
    let (progress, task) =
        secrets::add_to_builder(builder, secrets).create_detached_with_pull_progress()?;

    pull::wait_for_create(progress, task, on_pull).await
}

/// Fails with `NotFound` when no secret with the name is stored in the scope.
fn ensure_secret_exists(
    secrets: &[Secret],
    name: &str,
    sandbox: Option<&str>,
) -> Result<(), SandboxError> {
    if secrets.iter().any(|secret| secret.matches(name, sandbox)) {
        return Ok(());
    }

    Err(SandboxError::not_found(match sandbox {
        Some(sandbox) => format!("secret {name} doesn't exist in sandbox {sandbox}"),
        None => format!("secret {name} doesn't exist"),
    }))
}

/// Names a secret and its scope for the log.
fn describe_secret(name: &str, sandbox: Option<&str>) -> String {
    match sandbox {
        Some(sandbox) => format!("{name} for sandbox {sandbox}"),
        None => name.to_string(),
    }
}

/// Returns the change that removes the secret from its scope. A sandbox that loses its
/// sandbox-scoped secret gets the global secret with the same name instead, if there is one.
fn removal<'a>(stored: &'a [Secret], name: &'a str, sandbox: Option<&str>) -> SecretChange<'a> {
    let global = stored.iter().find(|secret| secret.matches(name, None));

    match (sandbox, global) {
        (Some(_), Some(global)) => SecretChange::Add(global),
        _ => SecretChange::Remove(name),
    }
}

/// Returns the sandbox a sandbox-scoped secret belongs to, or `None` for a global secret.
/// Fails with an error that names the sandbox when it doesn't exist.
async fn scoped_sandbox(sandbox: Option<&str>) -> Result<Option<SandboxHandle>, SandboxError> {
    let Some(name) = sandbox else {
        return Ok(None);
    };

    get_sandbox(name).await.map(Some).map_err(|err| match err {
        SandboxError::NotFound(_) => {
            SandboxError::not_found(format!("sandbox {name} doesn't exist"))
        }
        err => err,
    })
}

/// Returns the sandbox with the name.
async fn get_sandbox(name: &str) -> Result<SandboxHandle, SandboxError> {
    Sandbox::get(name).await.map_err(|err| match err {
        MicrosandboxError::SandboxNotFound(_) => {
            SandboxError::not_found("couldn't find specified sandbox")
        }
        err => {
            tracing::error!(error = ?err, "failed to get sandbox {name}");
            SandboxError::internal("failed to get sandbox")
        }
    })
}

/// Logs why connecting to a sandbox failed and returns the error the client sees. A stopped
/// sandbox is a normal situation, so only other failures are logged as errors.
fn connect_failed(name: &str, err: &MicrosandboxError) -> SandboxError {
    if matches!(err, MicrosandboxError::SandboxNotRunning(_)) {
        tracing::warn!(error = ?err, "failed to connect to sandbox {name}");
    } else {
        tracing::error!(error = ?err, "failed to connect to sandbox {name}");
    }

    SandboxError::failed_precondition("sandbox is not running")
}

/// Returns the error for a failed remove, logging failures other than a running sandbox.
fn remove_failed(name: &str, err: MicrosandboxError) -> SandboxError {
    match err {
        MicrosandboxError::SandboxStillRunning(_) => SandboxError::failed_precondition(format!(
            "sandbox {name} is running; stop it first or remove it with force"
        )),
        err => {
            tracing::error!(error = ?err, "failed to remove sandbox {name}");
            SandboxError::internal("failed to remove sandbox")
        }
    }
}

/// Logs why a forced remove couldn't stop the sandbox and returns the error the client sees.
fn stop_before_remove_failed(name: &str, err: &MicrosandboxError) -> SandboxError {
    tracing::error!(error = ?err, "failed to stop sandbox {name} before removing it");
    SandboxError::internal("failed to stop sandbox before removing it")
}

/// Stops the sandbox gracefully, and kills it when it hasn't stopped within `timeout`.
pub async fn stop_or_kill(sb: &SandboxHandle, timeout: Duration) -> Result<(), MicrosandboxError> {
    match sb.stop_with_timeout(timeout).await {
        Err(MicrosandboxError::StopTimeout { .. }) => {
            tracing::warn!(
                "sandbox {} didn't stop within {timeout:?}; killing it",
                sb.name()
            );
            sb.kill().await
        }
        result => result,
    }
}

/// Returns the status of the sandbox, see [`observed_status`]. Fails when the runtime doesn't
/// report whether the sandbox is paused, instead of guessing.
async fn current_status(sb: &SandboxHandle) -> Result<SandboxStatus, SandboxError> {
    observed_status(sb).await?.ok_or_else(|| {
        SandboxError::internal(format!(
            "the runtime of sandbox {} didn't report whether it's paused; try again, or stop \
             and start the sandbox",
            sb.name()
        ))
    })
}

/// Returns the status of the sandbox. microsandbox reports a paused sandbox as running when its
/// runtime doesn't say within 250 ms that it's paused, which happens on a busy host, so this
/// asks the runtime of a running sandbox again and waits up to [`PAUSE_STATE_TIMEOUT`]. Returns
/// `None` when the runtime doesn't answer in time.
async fn observed_status(sb: &SandboxHandle) -> Result<Option<SandboxStatus>, SandboxError> {
    match timeout(PAUSE_STATE_TIMEOUT, ask_pause_state(sb)).await {
        Ok(status) => status.map(Some),
        Err(_) => {
            tracing::error!(
                "the runtime of sandbox {} didn't report within {PAUSE_STATE_TIMEOUT:?} whether \
                 it's paused",
                sb.name()
            );
            Ok(None)
        }
    }
}

/// Returns the info of a listed sandbox, without a status when its runtime doesn't report
/// whether it's paused, or `None` when it was removed after it was listed.
async fn listed_info(sb: &SandboxHandle) -> Result<Option<SandboxInfo>, SandboxError> {
    match observed_status(sb).await {
        Ok(status) => Ok(Some(SandboxInfo::of(sb, status))),
        Err(SandboxError::NotFound(_)) => {
            tracing::info!("sandbox {} was removed while listing it", sb.name());
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// Asks the runtime of a running sandbox whether it's paused until it answers or the sandbox
/// stops. microsandbox closes its connection to the runtime when a request fails, and only
/// the next request opens a new one, so a failed request is retried. Each retry gets the
/// sandbox again, because it may have stopped, been removed or been replaced in the meantime,
/// and microsandbox refuses requests through the handle of a replaced sandbox.
async fn ask_pause_state(sb: &SandboxHandle) -> Result<SandboxStatus, SandboxError> {
    if let Some(status) = try_pause_state(sb).await {
        return Ok(status);
    }

    loop {
        sleep(PAUSE_STATE_RETRY_INTERVAL).await;

        if let Some(status) = try_pause_state(&get_sandbox(sb.name()).await?).await {
            return Ok(status);
        }
    }
}

/// Returns the status of the sandbox, asking its runtime once whether it's paused when
/// microsandbox reports it as running. Returns `None` when that request fails.
async fn try_pause_state(sb: &SandboxHandle) -> Option<SandboxStatus> {
    let status = sb.status_snapshot();

    if status != SandboxStatus::Running {
        return Some(status);
    }

    let name = sb.name();

    match sb.pause_state().await {
        Ok(state) if state.paused || state.recovery_required => Some(SandboxStatus::Paused),
        Ok(_) => Some(SandboxStatus::Running),
        // A runtime without a control endpoint, such as one whose control listener failed to
        // start, can't be paused either, because pausing goes through the same endpoint.
        Err(err @ MicrosandboxError::Runtime(_)) => {
            tracing::debug!("sandbox {name} runs without a pause control: {err}");
            Some(SandboxStatus::Running)
        }
        Err(err) => {
            tracing::debug!("failed to read the pause state of sandbox {name}: {err}");
            None
        }
    }
}

/// Whether microsandbox refuses to remove a sandbox with the status because it's still running.
fn is_live(status: SandboxStatus) -> bool {
    matches!(
        status,
        SandboxStatus::Starting
            | SandboxStatus::Running
            | SandboxStatus::Draining
            | SandboxStatus::Paused
    )
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

/// A change to the ports stored with a sandbox.
struct PortsChange {
    /// The ports stored before the change.
    stored: Vec<PortMapping>,
    /// The ports after the change.
    ports: Vec<PortMapping>,
    /// The forward the change asks for, if any.
    requested: Option<PortMapping>,
}

impl PortsChange {
    /// Whether the change opens the forward: it's the requested one, which must open even when
    /// it's stored already, or it isn't stored. A stored forward that already failed to open
    /// doesn't block other changes.
    fn is_new(&self, port: PortMapping) -> bool {
        self.requested == Some(port) || !self.stored.contains(&port)
    }
}

/// Returns the ports stored with the sandbox.
fn stored_ports(sb: &SandboxHandle) -> Vec<PortMapping> {
    sb.config()
        .ok()
        .and_then(|config| config.spec.labels.get(forward::PORTS_LABEL).cloned())
        .map(|value| forward::ports_from_label(&value))
        .unwrap_or_default()
}

/// Stores the ports with the sandbox like [`save_ports`], logging a warning when that fails.
async fn store_ports(sb: &SandboxHandle, ports: &[PortMapping]) {
    if let Err(err) = save_ports(sb, ports).await {
        tracing::warn!("{err}");
    }
}

/// Stores the ports with the sandbox when they differ from the stored ones. The label doesn't
/// affect the running VM, so it's applied on the next start rather than restarting the sandbox.
async fn save_ports(sb: &SandboxHandle, ports: &[PortMapping]) -> Result<(), SandboxError> {
    if stored_ports(sb) == ports {
        return Ok(());
    }

    sb.modify()
        .label(forward::PORTS_LABEL, forward::label_value(ports))
        .next_start()
        .apply()
        .await
        .map_err(|err| {
            tracing::error!(error = ?err, "failed to store the ports of sandbox {}", sb.name());
            SandboxError::internal(format!(
                "failed to store the ports of sandbox {}",
                sb.name()
            ))
        })?;

    Ok(())
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

/// Returns every sandbox, following the list cursor across pages.
async fn list_all_sandboxes() -> Result<Vec<SandboxHandle>, SandboxError> {
    list_all().await.map_err(|err| {
        tracing::error!(error = ?err, "failed to list sandboxes");
        SandboxError::internal("failed to list sandboxes")
    })
}

/// How often a listing starts over when a sandbox disappears while it runs.
const LIST_ATTEMPTS: u32 = 5;

/// Returns every sandbox in the microsandbox home, following the list cursor across pages.
///
/// microsandbox reloads each sandbox of a page after reading the page, so a sandbox that
/// another process removes in between fails the listing with `SandboxNotFound`. The listing
/// then starts over, up to [`LIST_ATTEMPTS`] times.
pub async fn list_all() -> Result<Vec<SandboxHandle>, MicrosandboxError> {
    retry_while_not_found(list_pages).await
}

/// Lists every page of sandboxes once.
async fn list_pages() -> Result<Vec<SandboxHandle>, MicrosandboxError> {
    let mut sandboxes = vec![];
    let mut next_cursor: Option<String> = None;

    loop {
        let result = Sandbox::list_with(|opt| match next_cursor.take() {
            Some(cursor) => opt.cursor(cursor),
            None => opt,
        })
        .await?;

        sandboxes.extend(result.sandboxes);

        if result.next_cursor.is_none() {
            break;
        }

        next_cursor = result.next_cursor;
    }

    Ok(sandboxes)
}

/// Runs the listing again while it fails because a sandbox disappeared during it, up to
/// [`LIST_ATTEMPTS`] times in total.
async fn retry_while_not_found<T, F, Fut>(mut listing: F) -> Result<T, MicrosandboxError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, MicrosandboxError>>,
{
    let mut attempt = 1;

    loop {
        match listing().await {
            Err(MicrosandboxError::SandboxNotFound(name)) if attempt < LIST_ATTEMPTS => {
                tracing::debug!(
                    "sandbox {name} disappeared while listing sandboxes; listing again"
                );
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// A change to the secrets of existing sandboxes.
enum SecretChange<'a> {
    Add(&'a Secret),
    Remove(&'a str),
}

impl fmt::Display for SecretChange<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretChange::Add(secret) => write!(f, "add secret {} to", secret.name()),
            SecretChange::Remove(name) => write!(f, "remove secret {name} from"),
        }
    }
}

/// Applies a secret change to every sandbox firebrick created, which are the ones with a host
/// name, except the `skipped` ones. Returns the names of the sandboxes the change failed for.
async fn update_firebrick_sandboxes(
    change: SecretChange<'_>,
    skipped: &HashSet<String>,
) -> Result<Vec<String>, SandboxError> {
    let mut failed = vec![];

    for handle in list_all_sandboxes().await? {
        if ssh::hostname_of(&handle).is_none() || skipped.contains(handle.name()) {
            continue;
        }

        failed.extend(apply_to_sandbox(&handle, &change).await);
    }

    Ok(failed)
}

/// Applies a secret change to one sandbox. Returns its name when the change failed.
async fn apply_to_sandbox(handle: &SandboxHandle, change: &SecretChange<'_>) -> Vec<String> {
    let result = match change {
        SecretChange::Add(secret) => secrets::add_to_sandbox(handle, secret).await,
        SecretChange::Remove(name) => secrets::remove_from_sandbox(handle, name).await,
    };

    match result {
        Ok(()) => vec![],
        Err(err) => {
            tracing::warn!(error = ?err, "failed to {change} sandbox {}", handle.name());
            vec![handle.name().to_string()]
        }
    }
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
    let sandboxes = match list_all_sandboxes().await {
        Ok(sandboxes) => sandboxes,
        Err(err) => {
            tracing::warn!("failed to update SSH config: {err}");
            return;
        }
    };
    let hostnames: Vec<String> = sandboxes.iter().filter_map(ssh::hostname_of).collect();

    if let Err(err) = ssh::sync_config(&hostnames) {
        tracing::warn!("failed to update SSH config: {err:#}");
    }

    sync_editor_settings(&hostnames);
    sync_zed_settings(&sandboxes);
}

/// Syncs the host names into the editors' Remote-SSH settings, logging a warning per editor
/// that fails.
fn sync_editor_settings(hostnames: &[String]) {
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

/// Syncs the sandboxes with a host name into Zed's remote projects, logging a warning when
/// that fails.
fn sync_zed_settings(sandboxes: &[SandboxHandle]) {
    let Some(config_root) = zed::config_root() else {
        tracing::warn!("failed to update Zed settings: HOME is not set");
        return;
    };
    let projects: Vec<zed::RemoteProject> = sandboxes.iter().filter_map(remote_project).collect();

    if let Err(err) = zed::sync_settings(&config_root, &projects) {
        tracing::warn!(
            "failed to update Zed settings: {:#}",
            anyhow::Error::from(err)
        );
    }
}

/// Returns the sandbox as a Zed remote project, or `None` when it has no host name.
fn remote_project(handle: &SandboxHandle) -> Option<zed::RemoteProject> {
    Some(zed::RemoteProject {
        host: ssh::hostname_of(handle)?,
        nickname: handle.name().to_string(),
        workspace_path: workspace_path_of(handle),
    })
}

/// Returns the guest path for a workspace: `/workspaces/<leaf-name>` of the absolute host path.
fn workspace_mount_path(workspace: &str) -> Result<String, SandboxError> {
    let path = Path::new(workspace);

    if !path.is_absolute() {
        return Err(SandboxError::invalid_argument(
            "workspace must be an absolute path",
        ));
    }

    let leaf = path.file_name().ok_or_else(|| {
        SandboxError::invalid_argument("workspace must not be the root directory")
    })?;

    Ok(format!("/workspaces/{}", leaf.to_string_lossy()))
}

/// Fails with `InvalidArgument` when a mount has a relative host path, an invalid guest path
/// (see [`firebrick_spec::guest_mount_path`]), or the guest path of the workspace or the Docker
/// data volume.
fn check_mounts(mounts: &[MountSpec], workspace_guest_path: &str) -> Result<(), SandboxError> {
    mounts
        .iter()
        .try_for_each(|mount| check_mount(mount, workspace_guest_path))
}

/// Checks the paths of one extra mount, see [`check_mounts`].
fn check_mount(mount: &MountSpec, workspace_guest_path: &str) -> Result<(), SandboxError> {
    if !Path::new(&mount.host).is_absolute() {
        return Err(SandboxError::invalid_argument(format!(
            "mount host path {} must be an absolute path",
            mount.host
        )));
    }

    let guest = firebrick_spec::guest_mount_path(&mount.guest)
        .map_err(|err| SandboxError::invalid_argument(format!("mount {}: {err}", mount.guest)))?;

    if guest == workspace_guest_path {
        return Err(SandboxError::invalid_argument(format!(
            "mount {guest} conflicts with the workspace mount"
        )));
    }

    if guest == DOCKER_DATA_PATH {
        return Err(SandboxError::invalid_argument(format!(
            "mount {guest} conflicts with the Docker data volume"
        )));
    }

    if guest == mise::DOTFILES_PATH {
        return Err(SandboxError::invalid_argument(format!(
            "mount {guest} conflicts with the mise dotfiles mount"
        )));
    }

    Ok(())
}

/// Returns the read-only mount of the dotfiles directory at [`mise::DOTFILES_PATH`], failing
/// with `InvalidArgument` when its host path is relative.
fn dotfiles_mount(host: Option<&str>) -> Result<Option<MountSpec>, SandboxError> {
    let Some(host) = host else {
        return Ok(None);
    };

    if !Path::new(host).is_absolute() {
        return Err(SandboxError::invalid_argument(format!(
            "mise config path {host} must be an absolute path"
        )));
    }

    Ok(Some(MountSpec {
        host: host.to_string(),
        guest: mise::DOTFILES_PATH.to_string(),
        readonly: true,
    }))
}

/// Bind mounts the extra host directories like the workspace: owned by the agent user, with
/// guest permission changes mirrored to the host, and read-only when the mount asks for it.
fn with_mounts(builder: SandboxBuilder, mounts: &[MountSpec]) -> SandboxBuilder {
    mounts.iter().fold(builder, |builder, mount| {
        builder.volume(&mount.guest, |m| bind_mount(m, mount))
    })
}

/// Configures a bind mount of the extra host directory.
fn bind_mount(builder: MountBuilder, mount: &MountSpec) -> MountBuilder {
    let builder = builder
        .bind(&mount.host)
        .host_permissions(HostPermissions::Mirror)
        .owner(1000, 1000);

    if mount.readonly {
        builder.readonly()
    } else {
        builder
    }
}

/// Returns the requested image, or the default image when the request doesn't name one.
fn sandbox_image(image: &str) -> &str {
    if image.is_empty() {
        firebrick_spec::DEFAULT_IMAGE
    } else {
        image
    }
}

/// Path of the init that runs as PID 1 in sandboxes with `init` enabled.
const INIT_PATH: &str = "/sbin/init";

/// Gives the sandbox its vCPUs, memory and Docker volume.
fn with_resources(builder: SandboxBuilder, resources: Resources) -> SandboxBuilder {
    builder
        .cpus(resources.cpus)
        .memory(resources.memory_mib)
        // A private ext4 disk, because Docker's overlayfs storage can't sit on the overlayfs
        // root. It lives until the sandbox is removed.
        .volume(DOCKER_DATA_PATH, |m| {
            m.owned_with(|v| v.disk().size(resources.docker_volume_mib))
        })
}

/// Hands PID 1 to the image's `/sbin/init` when `init` is enabled.
fn with_init(builder: SandboxBuilder, init: bool) -> SandboxBuilder {
    if init {
        builder.init(INIT_PATH)
    } else {
        builder
    }
}

/// Returns the error for a failed create. A sandbox whose init failed to boot is removed, so
/// a start with `init: false` can create it again.
async fn create_failed(name: &str, image: &str, err: &MicrosandboxError) -> SandboxError {
    let error = create_error(image, err);
    if matches!(error, SandboxError::FailedPrecondition(_))
        && let Err(err) = Sandbox::remove(name).await
    {
        tracing::warn!("failed to remove sandbox {name} after a failed create: {err}");
    }

    error
}

/// Turns a failed create of a sandbox from `image` into the error the client sees.
///
/// microsandbox only reports a missing init as text, so this matches on its message; other
/// failures that aren't about pulling the image stay internal.
fn create_error(image: &str, err: &MicrosandboxError) -> SandboxError {
    match err {
        MicrosandboxError::Image(ImageError::Registry(err)) => registry_error(image, err),
        err if err.to_string().contains("handoff failed") => SandboxError::failed_precondition(
            "failed to create sandbox: the image has no /sbin/init; add one or set init: false \
             in .firebrick.yml",
        ),
        _ => SandboxError::internal("failed to create sandbox"),
    }
}

/// Turns a registry error while pulling `image` into the error the client sees. Registries
/// such as Docker Hub and GHCR answer an unknown repository with "unauthorized", so they don't
/// reveal private ones, so that error can also mean the image doesn't exist.
fn registry_error(image: &str, err: &OciDistributionError) -> SandboxError {
    match err {
        OciDistributionError::UnauthorizedError { .. } => SandboxError::not_found(format!(
            "failed to create sandbox: image {image} doesn't exist, or its registry needs a login"
        )),
        OciDistributionError::ImageManifestNotFoundError(_) => image_not_found(image),
        OciDistributionError::RegistryError { envelope, .. } if is_not_found(envelope) => {
            image_not_found(image)
        }
        OciDistributionError::RequestError(_) => SandboxError::unavailable(format!(
            "failed to create sandbox: couldn't reach the registry of image {image}; check the \
             network connection"
        )),
        _ => SandboxError::internal(format!(
            "failed to create sandbox: failed to pull image {image}"
        )),
    }
}

/// Returns the error for an image the registry doesn't have.
fn image_not_found(image: &str) -> SandboxError {
    SandboxError::not_found(format!(
        "failed to create sandbox: image {image} doesn't exist"
    ))
}

/// Whether the registry's errors say that the repository or the tag doesn't exist.
fn is_not_found(envelope: &OciEnvelope) -> bool {
    envelope.errors.iter().any(|error| {
        matches!(
            error.code,
            OciErrorCode::ManifestUnknown | OciErrorCode::NameUnknown | OciErrorCode::NotFound
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::HostTools;
    use crate::open::HostOpener;
    use crate::vsock::test_endpoint_dir;

    /// Returns the host path as an absolute path of the platform: unchanged on Unix, and on the
    /// `C:` drive on Windows, where a path without a drive isn't absolute.
    fn host(path: &str) -> String {
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

    /// Returns a listing that fails with each error in turn, then succeeds, and counts its calls.
    fn listing_failing_with(
        errors: Vec<MicrosandboxError>,
    ) -> (
        Arc<std::sync::atomic::AtomicU32>,
        impl FnMut() -> std::future::Ready<Result<(), MicrosandboxError>>,
    ) {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = calls.clone();
        let mut errors = errors.into_iter();
        let listing = move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(errors.next().map_or(Ok(()), Err))
        };

        (calls, listing)
    }

    fn not_found() -> MicrosandboxError {
        MicrosandboxError::SandboxNotFound("removed".into())
    }

    #[tokio::test]
    async fn listing_starts_over_when_a_sandbox_disappears_during_it() {
        let (calls, listing) = listing_failing_with(vec![not_found(), not_found()]);

        assert!(retry_while_not_found(listing).await.is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn listing_gives_up_after_the_last_attempt() {
        let errors = (0..LIST_ATTEMPTS).map(|_| not_found()).collect();
        let (calls, listing) = listing_failing_with(errors);

        assert!(matches!(
            retry_while_not_found(listing).await,
            Err(MicrosandboxError::SandboxNotFound(_))
        ));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            LIST_ATTEMPTS
        );
    }

    #[tokio::test]
    async fn listing_returns_other_errors_without_retrying() {
        let error = MicrosandboxError::Runtime("broken".into());
        let (calls, listing) = listing_failing_with(vec![error]);

        assert!(matches!(
            retry_while_not_found(listing).await,
            Err(MicrosandboxError::Runtime(_))
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Returns a secret for `example.com` with the name, in the sandbox's scope or global.
    fn scoped_secret(name: &str, sandbox: Option<&str>) -> Secret {
        Secret::new(name.to_string(), "value".into(), vec!["example.com".into()])
            .unwrap()
            .in_scope(sandbox.map(str::to_string))
    }

    #[test]
    fn removal_puts_the_global_secret_back_in_a_sandbox() {
        let stored = [scoped_secret("X", None), scoped_secret("X", Some("a"))];

        assert!(matches!(
            removal(&stored, "X", Some("a")),
            SecretChange::Add(secret) if secret == &stored[0]
        ));
        assert!(matches!(
            removal(&stored[1..], "X", Some("a")),
            SecretChange::Remove("X")
        ));
        assert!(matches!(
            removal(&stored, "X", None),
            SecretChange::Remove("X")
        ));
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

    /// Returns the settings of a new sandbox created for a request with the image and the
    /// clipboard setting.
    fn new_settings(image: &str, clipboard: bool) -> SandboxSettings {
        new_settings_with_env(image, clipboard, None)
    }

    /// Returns the settings of a new sandbox created for a request with the image, the
    /// clipboard setting and the environment variables.
    fn new_settings_with_env(
        image: &str,
        clipboard: bool,
        env: Option<&BTreeMap<String, String>>,
    ) -> SandboxSettings {
        let request = StartSandbox {
            name: "dev",
            workspace: "/home/user/project",
            image,
            init: true,
            mise: false,
            network: &NetworkSpec::default(),
            ports: None,
            mounts: &[],
            dotfiles: None,
            clipboard,
            env,
            git_identity: None,
        };
        let resources = Resources {
            cpus: 1,
            memory_mib: 512,
            docker_volume_mib: 1024,
        };

        new_sandbox_settings(request, "/workspaces/project", "project.fbk", resources)
    }

    /// Returns the image label of a new sandbox created for a request with the image.
    fn new_image_label(image: &str) -> Option<String> {
        new_settings(image, false).labels.get(IMAGE_LABEL).cloned()
    }

    /// Returns the spec microsandbox would create the sandbox with, with its sockets in `/run`.
    async fn built_spec(settings: &SandboxSettings) -> SandboxSpec {
        let netlogs = NetLogs::new(
            Some(test_endpoint_dir(Path::new("/run/fbk"), "netlog")),
            None,
        );
        let clipboards = Clipboards::new(
            Some(test_endpoint_dir(Path::new("/run/fbk"), "clipboard")),
            Arc::new(HostTools::new(None, None)),
        );

        settings
            .builder(&netlogs, &clipboards)
            .image("ubuntu:26.04")
            .build()
            .await
            .unwrap()
            .spec
    }

    #[tokio::test]
    async fn new_sandbox_gets_the_requested_env_and_its_names() {
        let env = BTreeMap::from([
            ("GREETING".to_string(), "$HOME".to_string()),
            ("PORT".to_string(), "3000".to_string()),
        ]);

        let spec = built_spec(&new_settings_with_env("", false, Some(&env))).await;

        assert_eq!(env::stored(&spec), env);
    }

    #[tokio::test]
    async fn new_sandbox_without_env_gets_no_variables_from_the_spec() {
        let spec = built_spec(&new_settings("", false)).await;

        assert!(env::stored(&spec).is_empty());
        assert_eq!(
            spec.labels.get(env::NAMES_LABEL).map(String::as_str),
            Some("")
        );
    }

    #[tokio::test]
    async fn new_sandbox_with_the_clipboard_gets_its_label_and_route() {
        let settings = new_settings("", true);
        let spec = built_spec(&settings).await;
        let clipboards = Clipboards::new(
            Some(test_endpoint_dir(Path::new("/run/fbk"), "clipboard")),
            Arc::new(HostTools::new(None, None)),
        );

        assert_eq!(
            settings.labels.get(clipboard::ENABLED_LABEL),
            Some(&"true".to_string())
        );
        assert!(clipboard::can_copy(&settings.labels));
        assert!(clipboards.has_route("dev", &spec));
    }

    #[tokio::test]
    async fn new_sandbox_without_the_clipboard_gets_neither_label_nor_route() {
        let settings = new_settings("", false);
        let spec = built_spec(&settings).await;
        let ports: Vec<u32> = spec.vsock.routes.iter().map(|route| route.port).collect();

        assert_eq!(settings.labels.get(clipboard::ENABLED_LABEL), None);
        assert_eq!(settings.labels.get(clipboard::COPY_LABEL), None);
        assert_eq!(ports, [crate::netlog::GUEST_PORT]);
        assert!(!spec.runtime.scripts.contains_key(clipboard::MARKER));
    }

    #[test]
    fn new_sandbox_records_the_default_image_by_name() {
        assert_eq!(
            new_image_label(""),
            Some(firebrick_spec::DEFAULT_IMAGE.to_string())
        );
    }

    #[test]
    fn new_sandbox_records_the_requested_image() {
        assert_eq!(
            new_image_label("alpine:3.22"),
            Some("alpine:3.22".to_string())
        );
    }

    #[test]
    fn sandbox_image_falls_back_to_default() {
        assert_eq!(sandbox_image("alpine:3.22"), "alpine:3.22");
        assert_eq!(sandbox_image(""), firebrick_spec::DEFAULT_IMAGE);
    }

    const IMAGE: &str = "alpine:0.0.404";

    /// Returns the error of a pull whose registry answered with the error code.
    fn registry_error_with_code(code: &str) -> MicrosandboxError {
        let envelope = serde_yaml::from_str(&format!("errors: [{{code: {code}}}]")).unwrap();

        MicrosandboxError::Image(ImageError::Registry(OciDistributionError::RegistryError {
            envelope,
            url: "https://registry.example/v2/alpine/manifests/0.0.404".to_string(),
        }))
    }

    #[test]
    fn create_error_explains_missing_init() {
        let error = create_error(
            IMAGE,
            &MicrosandboxError::Custom(
                "guest initialization failed: handoff failed: init error: no init binary found"
                    .to_string(),
            ),
        );

        assert!(
            matches!(&error, SandboxError::FailedPrecondition(message) if message.contains("set init: false")),
            "{error:?}"
        );
    }

    #[test]
    fn create_error_hides_other_failures() {
        assert_eq!(
            create_error(IMAGE, &MicrosandboxError::Custom("boom".to_string())),
            SandboxError::internal("failed to create sandbox")
        );
    }

    #[test]
    fn create_error_names_an_unknown_image() {
        for code in ["MANIFEST_UNKNOWN", "NAME_UNKNOWN", "NOT_FOUND"] {
            assert_eq!(
                create_error(IMAGE, &registry_error_with_code(code)),
                SandboxError::not_found(
                    "failed to create sandbox: image alpine:0.0.404 doesn't exist"
                ),
                "{code}"
            );
        }
    }

    #[test]
    fn create_error_names_an_image_the_registry_refuses() {
        let err = MicrosandboxError::Image(ImageError::Registry(
            OciDistributionError::UnauthorizedError {
                url: "https://ghcr.io/v2/user/private/manifests/1".to_string(),
            },
        ));

        assert_eq!(
            create_error(IMAGE, &err),
            SandboxError::not_found(
                "failed to create sandbox: image alpine:0.0.404 doesn't exist, or its registry \
                 needs a login"
            )
        );
    }

    #[test]
    fn create_error_names_the_image_of_other_registry_errors() {
        assert_eq!(
            create_error(IMAGE, &registry_error_with_code("DENIED")),
            SandboxError::internal("failed to create sandbox: failed to pull image alpine:0.0.404")
        );
    }

    #[test]
    fn workspace_mount_path_uses_leaf_name() {
        assert_eq!(
            workspace_mount_path(&host("/home/user/project")).unwrap(),
            "/workspaces/project"
        );
        assert_eq!(
            workspace_mount_path(&host("/home/user/project/")).unwrap(),
            "/workspaces/project"
        );
    }

    #[test]
    fn workspace_mount_path_rejects_relative_empty_and_root_paths() {
        for workspace in ["", "project", "./project", "/"] {
            assert!(
                matches!(
                    workspace_mount_path(workspace),
                    Err(SandboxError::InvalidArgument(_))
                ),
                "{workspace:?} should be rejected"
            );
        }
    }

    fn mount(host: &str, guest: &str) -> MountSpec {
        MountSpec {
            host: host.to_string(),
            guest: guest.to_string(),
            readonly: false,
        }
    }

    #[test]
    fn check_mounts_accepts_absolute_paths() {
        let mounts = [mount(&host("/home/user/lib"), "/workspaces/lib")];

        assert_eq!(check_mounts(&mounts, "/workspaces/project"), Ok(()));
    }

    #[test]
    fn check_mounts_rejects_the_workspace_guest_path() {
        let mounts = [mount(&host("/home/user/lib"), "/workspaces/project")];

        assert_eq!(
            check_mounts(&mounts, "/workspaces/project"),
            Err(SandboxError::InvalidArgument(
                "mount /workspaces/project conflicts with the workspace mount".into()
            ))
        );
    }

    #[test]
    fn check_mounts_rejects_the_workspace_guest_path_in_another_form() {
        let mounts = [mount(&host("/home/user/lib"), "/workspaces/project/.")];

        assert_eq!(
            check_mounts(&mounts, "/workspaces/project"),
            Err(SandboxError::InvalidArgument(
                "mount /workspaces/project conflicts with the workspace mount".into()
            ))
        );
    }

    #[test]
    fn check_mounts_rejects_the_docker_data_path() {
        let mounts = [mount(&host("/home/user/docker"), "/var/lib/docker/")];

        assert_eq!(
            check_mounts(&mounts, "/workspaces/project"),
            Err(SandboxError::InvalidArgument(
                "mount /var/lib/docker conflicts with the Docker data volume".into()
            ))
        );
    }

    #[test]
    fn check_mounts_rejects_the_dotfiles_path() {
        let mounts = [mount(
            &host("/home/user/dotfiles"),
            "/opt/firebrick/dotfiles/",
        )];

        assert_eq!(
            check_mounts(&mounts, "/workspaces/project"),
            Err(SandboxError::InvalidArgument(
                "mount /opt/firebrick/dotfiles conflicts with the mise dotfiles mount".into()
            ))
        );
    }

    #[test]
    fn dotfiles_mount_is_read_only_at_the_dotfiles_path() {
        let dotfiles = host("/home/user/dotfiles");

        assert_eq!(
            dotfiles_mount(Some(&dotfiles)),
            Ok(Some(MountSpec {
                host: dotfiles.clone(),
                guest: "/opt/firebrick/dotfiles".to_string(),
                readonly: true,
            }))
        );
        assert_eq!(dotfiles_mount(None), Ok(None));
    }

    #[test]
    fn dotfiles_mount_rejects_a_relative_host_path() {
        assert!(matches!(
            dotfiles_mount(Some("dotfiles")),
            Err(SandboxError::InvalidArgument(_))
        ));
    }

    #[test]
    fn check_mounts_rejects_relative_and_invalid_paths() {
        for mounts in [
            [mount("../lib", "/lib")],
            [mount(&host("/home/user/lib"), "lib")],
            [mount(&host("/home/user/lib"), "/")],
            [mount(&host("/home/user/lib"), "/lib/../etc")],
            [mount(&host("/home/user/lib"), "/lib:v1")],
        ] {
            assert!(
                matches!(
                    check_mounts(&mounts, "/workspaces/project"),
                    Err(SandboxError::InvalidArgument(_))
                ),
                "{mounts:?} should be rejected"
            );
        }
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

    #[test]
    fn ensure_secret_exists_checks_the_scope() {
        let global = scoped_secret("X", None);
        let scoped = scoped_secret("X", Some("a"));

        assert_eq!(
            ensure_secret_exists(std::slice::from_ref(&scoped), "X", Some("a")),
            Ok(())
        );
        assert_eq!(
            ensure_secret_exists(std::slice::from_ref(&global), "X", None),
            Ok(())
        );
        assert_eq!(
            ensure_secret_exists(&[global], "X", Some("a")),
            Err(SandboxError::NotFound(
                "secret X doesn't exist in sandbox a".into()
            ))
        );
        assert_eq!(
            ensure_secret_exists(&[scoped], "X", None),
            Err(SandboxError::NotFound("secret X doesn't exist".into()))
        );
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

    #[test]
    fn is_in_use_matches_only_microsandbox_in_use_error() {
        let in_use = MicrosandboxError::Custom(
            "snapshot is in use by an active operation: /msb/snapshots/demo/snap_1".to_string(),
        );

        assert!(is_in_use(&in_use));
        assert!(!is_in_use(&MicrosandboxError::Custom("boom".to_string())));
        assert!(!is_in_use(&MicrosandboxError::SnapshotNotFound(
            "snap_1".to_string()
        )));
    }
}
