//! The size of a sandbox's root disk: set when the sandbox is created, and grown when an
//! existing sandbox starts with a larger requested size. microsandbox can't shrink a root disk.

use firebrick_spec::{MIN_ROOT_DISK_MIB, VolumesSpec};
use microsandbox::sandbox::{RootDisk, RootfsSource, SandboxBuilder, SandboxHandle, SandboxSpec};

/// A root disk of an existing sandbox that didn't get the requested size. The sandbox still
/// starts with its current disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootDiskNotice {
    /// The requested size in MiB.
    pub requested_mib: u32,
    /// The size of the disk in MiB.
    pub current_mib: u32,
    /// Why the disk didn't get the requested size.
    pub reason: NoticeReason,
}

/// Why a root disk didn't get the requested size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeReason {
    /// The sandbox was already running; the disk grows when it restarts.
    PendingRestart,
    /// The requested size is smaller than the disk, which can't shrink.
    ShrinkUnsupported,
    /// Growing the disk failed with the error.
    GrowFailed(String),
}

/// What to do with the root disk of an existing sandbox for a requested size.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// The disk has the requested size.
    Keep,
    /// Grow the stopped disk to the requested size.
    Grow,
    /// Leave the disk alone for the reason.
    Notice(NoticeReason),
}

/// Returns the default size of the root disk in MiB.
pub fn default_mib() -> u32 {
    // The default size is a constant that parses; 16 GiB matches it should that ever change.
    firebrick_spec::parse_root_disk_mib(&VolumesSpec::default().root).unwrap_or(16 * 1024)
}

/// Gives a new sandbox a root disk of `size_mib`. The builder must have its image already,
/// because microsandbox only sizes the root disk of an OCI image.
pub(super) fn with_size(builder: SandboxBuilder, size_mib: u32) -> SandboxBuilder {
    builder.root_disk(size_mib)
}

/// Returns the size of the sandbox's root disk in MiB, or `None` when microsandbox can't resize
/// it: a tmpfs or a user-supplied disk image, or a root filesystem that isn't an OCI image. A
/// disk without a recorded size has microsandbox's default size, [`MIN_ROOT_DISK_MIB`].
pub(super) fn current_mib(spec: &SandboxSpec) -> Option<u32> {
    let RootfsSource::Oci(oci) = &spec.image else {
        return None;
    };

    match &oci.root_disk {
        None => Some(MIN_ROOT_DISK_MIB),
        Some(RootDisk::Managed { size_mib } | RootDisk::Flat { size_mib, .. }) => {
            Some(size_mib.unwrap_or(MIN_ROOT_DISK_MIB))
        }
        Some(_) => None,
    }
}

/// Grows the root disk of the stopped sandbox to `requested_mib` when it's smaller. Returns a
/// notice when the disk keeps another size than requested.
pub(super) async fn grow_stopped(sb: &SandboxHandle, requested_mib: u32) -> Option<RootDiskNotice> {
    let current_mib = current_of(sb)?;
    let reason = match action(current_mib, requested_mib, false) {
        Action::Keep => return None,
        Action::Notice(reason) => reason,
        Action::Grow => grow(sb, current_mib, requested_mib).await?,
    };

    Some(RootDiskNotice {
        requested_mib,
        current_mib,
        reason,
    })
}

/// Returns a notice when the running sandbox's root disk has another size than
/// `requested_mib`, which it keeps until it restarts.
pub(super) fn pending(sb: &SandboxHandle, requested_mib: u32) -> Option<RootDiskNotice> {
    let current_mib = current_of(sb)?;
    let Action::Notice(reason) = action(current_mib, requested_mib, true) else {
        return None;
    };

    Some(RootDiskNotice {
        requested_mib,
        current_mib,
        reason,
    })
}

/// Returns the size of the sandbox's root disk, logging a warning when its config can't be
/// read.
fn current_of(sb: &SandboxHandle) -> Option<u32> {
    match sb.config() {
        Ok(config) => current_mib(&config.spec),
        Err(err) => {
            tracing::warn!(error = ?err, "failed to read the root disk of sandbox {}", sb.name());
            None
        }
    }
}

/// Decides what to do with a root disk of `current_mib` when `requested_mib` is asked for.
fn action(current_mib: u32, requested_mib: u32, running: bool) -> Action {
    if requested_mib == current_mib {
        Action::Keep
    } else if requested_mib < current_mib {
        Action::Notice(NoticeReason::ShrinkUnsupported)
    } else if running {
        Action::Notice(NoticeReason::PendingRestart)
    } else {
        Action::Grow
    }
}

/// Grows the stopped sandbox's root disk, returning the reason with the error when that fails.
async fn grow(sb: &SandboxHandle, current_mib: u32, requested_mib: u32) -> Option<NoticeReason> {
    match sb.modify().root_disk_size(requested_mib).apply().await {
        Ok(_) => {
            tracing::info!(
                "grew the root disk of sandbox {} from {current_mib} MiB to {requested_mib} MiB",
                sb.name()
            );
            None
        }
        Err(err) => {
            tracing::warn!(error = ?err, "failed to grow the root disk of sandbox {}", sb.name());
            Some(NoticeReason::GrowFailed(err.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use microsandbox::Sandbox;

    /// Returns the spec of a sandbox built by `configure` from the `ubuntu:26.04` image.
    async fn spec_of(configure: impl FnOnce(SandboxBuilder) -> SandboxBuilder) -> SandboxSpec {
        configure(Sandbox::builder("root-disk-test").image("ubuntu:26.04"))
            .build()
            .await
            .unwrap()
            .spec
    }

    #[tokio::test]
    async fn new_sandbox_gets_the_size() {
        let spec = spec_of(|builder| with_size(builder, 16 * 1024)).await;

        assert_eq!(current_mib(&spec), Some(16 * 1024));
    }

    #[tokio::test]
    async fn sandbox_without_a_size_has_the_default() {
        let spec = spec_of(|builder| builder).await;

        assert_eq!(current_mib(&spec), Some(MIN_ROOT_DISK_MIB));
    }

    #[tokio::test]
    async fn tmpfs_root_disk_has_no_size() {
        let spec = spec_of(|builder| builder.root_disk_with(|d| d.tmpfs().size(256))).await;

        assert_eq!(current_mib(&spec), None);
    }

    #[test]
    fn same_size_keeps_the_disk() {
        assert_eq!(action(16384, 16384, false), Action::Keep);
        assert_eq!(action(16384, 16384, true), Action::Keep);
    }

    #[test]
    fn larger_size_grows_a_stopped_disk() {
        assert_eq!(action(4096, 16384, false), Action::Grow);
    }

    #[test]
    fn larger_size_is_pending_on_a_running_disk() {
        assert_eq!(
            action(4096, 16384, true),
            Action::Notice(NoticeReason::PendingRestart)
        );
    }

    #[test]
    fn smaller_size_is_refused() {
        for running in [false, true] {
            assert_eq!(
                action(16384, 8192, running),
                Action::Notice(NoticeReason::ShrinkUnsupported),
                "{running}"
            );
        }
    }
}
