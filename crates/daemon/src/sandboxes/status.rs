//! The status of sandboxes as microsandbox reports it, and listing them, retrying when a
//! sandbox disappears while microsandbox lists them.

use super::SandboxInfo;
use super::error::SandboxError;
use microsandbox::sandbox::{SandboxHandle, SandboxStatus};
use microsandbox::{MicrosandboxError, Sandbox};
use std::time::Duration;
use tokio::time::{sleep, timeout};

/// How long the runtime of a running sandbox gets to report whether the sandbox is paused.
const PAUSE_STATE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait before asking the runtime of a sandbox again whether it's paused.
const PAUSE_STATE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Fails with `FailedPrecondition` when the sandbox is paused, because an upgrade can't pause
/// it again.
pub(super) async fn refuse_paused(sb: &SandboxHandle) -> Result<(), SandboxError> {
    if current_status(sb).await? != SandboxStatus::Paused {
        return Ok(());
    }

    Err(SandboxError::failed_precondition(format!(
        "sandbox {} is paused; resume or stop it before upgrading",
        sb.name()
    )))
}

/// Returns the sandbox with the name.
pub(super) async fn get_sandbox(name: &str) -> Result<SandboxHandle, SandboxError> {
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
pub(super) async fn current_status(sb: &SandboxHandle) -> Result<SandboxStatus, SandboxError> {
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
pub(super) async fn listed_info(sb: &SandboxHandle) -> Result<Option<SandboxInfo>, SandboxError> {
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
pub(super) fn is_live(status: SandboxStatus) -> bool {
    matches!(
        status,
        SandboxStatus::Starting
            | SandboxStatus::Running
            | SandboxStatus::Draining
            | SandboxStatus::Paused
    )
}

/// Returns every sandbox, following the list cursor across pages.
pub(super) async fn list_all_sandboxes() -> Result<Vec<SandboxHandle>, SandboxError> {
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
pub(super) async fn retry_while_not_found<T, F, Fut>(mut listing: F) -> Result<T, MicrosandboxError>
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
}
