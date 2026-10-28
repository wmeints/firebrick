//! The settings a sandbox is created with: its image, resources, mounts, workspace, labels and
//! routes, read back from an existing sandbox when it's recreated.

use super::error::SandboxError;
use super::{IMAGE_LABEL, Resources, StartSandbox};
use crate::forward::{self};
use crate::guest::clipboard::{self, Clipboards};
use crate::guest::netlog::NetLogs;
use crate::sandboxes::provision::env;
use crate::sandboxes::provision::mise::{self};
use crate::ssh;
use firebrick_spec::{MountSpec, VolumesSpec};
use microsandbox::sandbox::{
    HostPermissions, MountBuilder, OwnedVolumeStorage, SandboxBuilder, SandboxHandle, SandboxSpec,
    VolumeMount,
};
use microsandbox::{MicrosandboxError, Sandbox};
use std::collections::BTreeMap;
use std::path::Path;

/// Guest path of the disk that holds Docker's data.
const DOCKER_DATA_PATH: &str = "/var/lib/docker";

/// Returns the guest path a sandbox mounts its workspace at, which is its working directory.
pub(super) fn workspace_path_of(handle: &SandboxHandle) -> Option<String> {
    handle.config().ok()?.spec.runtime.workdir
}

/// Returns the host directory a sandbox bind-mounts at its workspace path. microsandbox stores
/// the host path canonicalized when it creates the sandbox.
fn workspace_host_path_of(handle: &SandboxHandle) -> Option<String> {
    workspace_host_path_in(&handle.config().ok()?.spec)
}

/// Returns the host directory the sandbox config bind-mounts at its workspace path.
pub(super) fn workspace_host_path_in(spec: &SandboxSpec) -> Option<String> {
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
pub(super) struct SandboxSettings {
    pub(super) name: String,
    /// Labels such as the SSH host name and the mise setting.
    pub(super) labels: BTreeMap<String, String>,
    /// The host directory mounted as the workspace and its guest path, which is the workdir.
    pub(super) workspace: Option<Workspace>,
    /// Extra host directories bind-mounted next to the workspace.
    pub(super) mounts: Vec<MountSpec>,
    /// Environment variables from the spec, which [`env::NAMES_LABEL`] names.
    env: BTreeMap<String, String>,
    init: bool,
    resources: Resources,
}

/// A host directory bind-mounted into the sandbox as its working directory.
#[derive(Debug, Clone)]
pub(super) struct Workspace {
    host: String,
    pub(super) guest: String,
}

impl SandboxSettings {
    /// Reads the settings of an existing sandbox from the config microsandbox stores for it.
    pub(super) fn of(handle: &SandboxHandle) -> Result<Self, MicrosandboxError> {
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
    pub(super) fn builder(&self, netlogs: &NetLogs, clipboards: &Clipboards) -> SandboxBuilder {
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
pub(super) fn docker_volume_mib(mounts: &[VolumeMount]) -> u32 {
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

/// Returns the settings of a new sandbox for the request, with its workspace mounted at
/// `guest_path` and the SSH host name.
pub(super) fn new_sandbox_settings(
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

/// Returns the guest path for a workspace: `/workspaces/<leaf-name>` of the absolute host path.
pub(super) fn workspace_mount_path(workspace: &str) -> Result<String, SandboxError> {
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
pub(super) fn check_mounts(
    mounts: &[MountSpec],
    workspace_guest_path: &str,
) -> Result<(), SandboxError> {
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

    if guest == firebrick_spec::MISE_DOTFILES_PATH {
        return Err(SandboxError::invalid_argument(format!(
            "mount {guest} conflicts with the mise dotfiles mount"
        )));
    }

    Ok(())
}

/// Returns the read-only mount of the dotfiles directory, [`MountSpec::mise_dotfiles`], failing
/// with `InvalidArgument` when its host path is relative.
pub(super) fn dotfiles_mount(host: Option<&str>) -> Result<Option<MountSpec>, SandboxError> {
    let Some(host) = host else {
        return Ok(None);
    };

    if !Path::new(host).is_absolute() {
        return Err(SandboxError::invalid_argument(format!(
            "mise config path {host} must be an absolute path"
        )));
    }

    Ok(Some(MountSpec::mise_dotfiles(host)))
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
pub(super) fn sandbox_image(image: &str) -> &str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::clipboard::HostTools;
    use crate::guest::test_endpoint_dir;
    use crate::sandboxes::tests::host;
    use firebrick_spec::NetworkSpec;
    use std::sync::Arc;

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
        assert_eq!(ports, [crate::guest::netlog::GUEST_PORT]);
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
}
