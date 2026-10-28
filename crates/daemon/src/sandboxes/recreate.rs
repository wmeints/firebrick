//! Recreating a sandbox with a changed network policy or a newer image: snapshots the sandbox,
//! removes it and creates it again from the snapshot, and probes a pulled image for its digest
//! before an upgrade replaces the sandbox.

use super::error::{SandboxError, create_error};
use super::settings::{SandboxSettings, sandbox_image};
use super::status::{
    current_status, get_sandbox, is_live, list_all_sandboxes, refuse_paused, stop_or_kill,
};
use super::{
    IMAGE_LABEL, STOP_TIMEOUT, SandboxManager, Upgrade, image_reference, install_mise_tools,
    sync_ssh_config,
};
use crate::guest::clipboard;
use crate::sandboxes::provision::mise::{self};
use crate::sandboxes::provision::network;
use crate::sandboxes::provision::pull::{self, PullUpdate};
use crate::secrets::{self, Secret};
use firebrick_spec::NetworkSpec;
use microsandbox::sandbox::{PullPolicy, SandboxBuilder, SandboxHandle, SandboxStatus};
use microsandbox::snapshot::Snapshot;
use microsandbox::{MicrosandboxError, Sandbox};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{Instant, sleep};

impl SandboxManager {
    /// Waits up to `timeout` for the updates and upgrades of sandboxes that are running.
    /// Returns whether they finished. Used when the daemon shuts down, after it stopped taking
    /// requests.
    pub async fn finish_recreations(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.recreations.write())
            .await
            .is_ok()
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
}

/// How long deleting a snapshot waits for the operations that still read it.
const SNAPSHOT_IN_USE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long deleting a snapshot that's in use waits before it tries again.
const SNAPSHOT_RETRY_INTERVAL: Duration = Duration::from_millis(100);

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

/// Adds the egress rules, the label that records them and the secrets to the builder and
/// creates the detached sandbox, passing the progress of its image pull to `on_pull`. The
/// rules go first, because they replace the TLS settings that the secrets turn on.
pub(super) async fn create_with(
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

#[cfg(test)]
mod tests {
    use super::*;

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
