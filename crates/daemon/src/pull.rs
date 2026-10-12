//! Image pull progress of a sandbox that is being created: adds up the bytes microsandbox
//! reports per layer and turns them into throttled [`PullUpdate`]s. A pull from the cache
//! reports no layer downloads, so it produces no updates.

use microsandbox::sandbox::{PullProgress, PullProgressHandle};
use microsandbox::{MicrosandboxError, MicrosandboxResult, Sandbox};
use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Minimum time between two progress messages, except the last one of a pull.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Limits progress messages to one every [`PROGRESS_INTERVAL`].
#[derive(Debug, Default)]
pub struct Throttle {
    last_sent: Option<Instant>,
}

impl Throttle {
    /// Whether a message may be sent at `now`: no message was sent less than
    /// [`PROGRESS_INTERVAL`] before it.
    pub fn ready(&self, now: Instant) -> bool {
        self.last_sent
            .is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL)
    }

    /// Records that a message was sent at `now`.
    pub fn sent(&mut self, now: Instant) {
        self.last_sent = Some(now);
    }
}

/// How far the download of an image has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullUpdate {
    /// Reference of the image being pulled.
    pub image: String,
    /// Bytes downloaded so far, over all layers.
    pub downloaded_bytes: u64,
    /// Compressed size of all layers, or `None` when the manifest has no layer sizes.
    pub total_bytes: Option<u64>,
    /// Whether this is the last update of the pull.
    pub complete: bool,
}

/// Adds up the bytes of the layers of an image pull and decides when to report them.
#[derive(Debug, Default)]
pub struct PullTracker {
    image: String,
    total_bytes: Option<u64>,
    layer_bytes: HashMap<usize, u64>,
    downloading: bool,
    throttle: Throttle,
}

impl PullTracker {
    /// Records a pull event received at `now`. Returns the message to send, if any: one when a
    /// layer downloads and at least [`PROGRESS_INTERVAL`] passed since the last message, one
    /// for every layer that finishes, so the bar doesn't stall while the image is unpacked, and
    /// a final one with `complete` set when a pull that downloaded layers completes.
    pub fn update(&mut self, event: &PullProgress, now: Instant) -> Option<PullUpdate> {
        match event {
            PullProgress::Resolved {
                reference,
                total_download_bytes,
                ..
            } => self.resolve(reference, *total_download_bytes),
            PullProgress::LayerDownloadProgress {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                self.downloading = true;
                self.record(*layer_index, *downloaded_bytes);
                self.throttled(now)
            }
            // Also sent for layers that are cached, with their full size.
            PullProgress::LayerDownloadComplete {
                layer_index,
                downloaded_bytes,
                ..
            } => {
                self.record(*layer_index, *downloaded_bytes);
                self.sent(now)
            }
            PullProgress::Complete { .. } => self.downloading.then(|| self.message(true)),
            _ => None,
        }
    }

    /// Stores the image and its size from the manifest.
    fn resolve(&mut self, reference: &str, total_bytes: Option<u64>) -> Option<PullUpdate> {
        self.image = reference.to_string();
        self.total_bytes = total_bytes;
        None
    }

    /// Stores the bytes of a layer, never lowering them.
    fn record(&mut self, layer_index: usize, downloaded_bytes: u64) {
        let bytes = self.layer_bytes.entry(layer_index).or_default();
        *bytes = (*bytes).max(downloaded_bytes);
    }

    /// Returns a progress message unless the last one was sent less than
    /// [`PROGRESS_INTERVAL`] before `now`.
    fn throttled(&mut self, now: Instant) -> Option<PullUpdate> {
        if !self.throttle.ready(now) {
            return None;
        }

        self.sent(now)
    }

    /// Returns a progress message sent at `now`, once a layer downloads.
    fn sent(&mut self, now: Instant) -> Option<PullUpdate> {
        if !self.downloading {
            return None;
        }

        self.throttle.sent(now);
        Some(self.message(false))
    }

    fn message(&self, complete: bool) -> PullUpdate {
        PullUpdate {
            image: self.image.clone(),
            downloaded_bytes: self.layer_bytes.values().sum(),
            total_bytes: self.total_bytes,
            complete,
        }
    }
}

/// Waits for the sandbox that `task` creates and passes the progress of its image pull to
/// `on_progress` meanwhile. Progress events are best-effort, so the result only depends on
/// the task.
pub async fn wait_for_create(
    progress: PullProgressHandle,
    mut task: JoinHandle<MicrosandboxResult<Sandbox>>,
    mut on_progress: impl FnMut(PullUpdate) + Send,
) -> MicrosandboxResult<Sandbox> {
    let mut events = progress.into_receiver();
    let mut tracker = PullTracker::default();
    let mut report = |event: PullProgress| {
        if let Some(message) = tracker.update(&event, Instant::now()) {
            on_progress(message);
        }
    };

    loop {
        tokio::select! {
            Some(event) = events.recv() => report(event),
            result = &mut task => {
                // Report the events that arrived together with the result, such as `Complete`.
                while let Ok(event) = events.try_recv() {
                    report(event);
                }

                return result.map_err(|err| {
                    MicrosandboxError::Custom(format!("sandbox creation task failed: {err}"))
                })?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use microsandbox_image::PullProgressSender;
    use std::sync::Arc;

    const IMAGE: &str = "ghcr.io/wmeints/firebrick-base:test";

    fn resolved(total: Option<u64>) -> PullProgress {
        PullProgress::Resolved {
            reference: Arc::from(IMAGE),
            manifest_digest: Arc::from("sha256:abc"),
            layer_count: 2,
            total_download_bytes: total,
        }
    }

    fn layer_progress(layer_index: usize, downloaded_bytes: u64) -> PullProgress {
        PullProgress::LayerDownloadProgress {
            layer_index,
            digest: Arc::from("sha256:layer"),
            downloaded_bytes,
            total_bytes: None,
        }
    }

    fn layer_complete(layer_index: usize, downloaded_bytes: u64) -> PullProgress {
        PullProgress::LayerDownloadComplete {
            layer_index,
            digest: Arc::from("sha256:layer"),
            downloaded_bytes,
        }
    }

    fn complete() -> PullProgress {
        PullProgress::Complete {
            reference: Arc::from(IMAGE),
            layer_count: 2,
        }
    }

    fn progress(downloaded_bytes: u64, total_bytes: Option<u64>) -> PullUpdate {
        PullUpdate {
            image: IMAGE.to_string(),
            downloaded_bytes,
            total_bytes,
            complete: false,
        }
    }

    /// Feeds the events to a tracker, each one [`PROGRESS_INTERVAL`] after the previous.
    fn track(events: &[PullProgress]) -> Vec<PullUpdate> {
        let mut tracker = PullTracker::default();
        let start = Instant::now();

        (0u32..)
            .zip(events)
            .filter_map(|(step, event)| tracker.update(event, start + PROGRESS_INTERVAL * step))
            .collect()
    }

    #[test]
    fn cached_image_sends_no_progress() {
        let events = [
            PullProgress::Resolving {
                reference: Arc::from(IMAGE),
            },
            resolved(Some(300)),
            complete(),
        ];

        assert_eq!(track(&events), vec![]);
    }

    #[test]
    fn cached_layers_alone_send_no_progress() {
        let events = [resolved(Some(300)), layer_complete(0, 100), complete()];

        assert_eq!(track(&events), vec![]);
    }

    #[test]
    fn adds_up_the_bytes_of_the_layers() {
        let events = [
            resolved(Some(300)),
            layer_progress(0, 50),
            layer_progress(1, 20),
            layer_progress(0, 100),
            layer_complete(1, 200),
            complete(),
        ];

        assert_eq!(
            track(&events),
            vec![
                progress(50, Some(300)),
                progress(70, Some(300)),
                progress(120, Some(300)),
                progress(300, Some(300)),
                PullUpdate {
                    complete: true,
                    ..progress(300, Some(300))
                },
            ]
        );
    }

    #[test]
    fn counts_cached_layers_once_a_layer_downloads() {
        let events = [
            resolved(Some(300)),
            layer_complete(0, 100),
            layer_progress(1, 50),
        ];

        assert_eq!(track(&events), vec![progress(150, Some(300))]);
    }

    #[test]
    fn keeps_the_total_unset_when_the_manifest_has_no_sizes() {
        let events = [resolved(None), layer_progress(0, 50)];

        assert_eq!(track(&events), vec![progress(50, None)]);
    }

    /// Sends the events of a pull that downloads one layer, then fails like a create would.
    async fn fail_after_a_pull(sender: PullProgressSender) -> MicrosandboxResult<Sandbox> {
        sender.send(resolved(Some(300)));
        sender.send(layer_progress(0, 300));
        sender.send(complete());

        Err(MicrosandboxError::Custom("boom".to_string()))
    }

    #[tokio::test]
    async fn wait_for_create_reports_the_events_sent_with_the_result() {
        let (handle, sender) = microsandbox_image::progress_channel();
        let task = tokio::spawn(fail_after_a_pull(sender));
        let mut updates = vec![];

        let result = wait_for_create(handle, task, |update| updates.push(update)).await;

        assert_eq!(result.err().map(|err| err.to_string()), Some("boom".into()));
        assert_eq!(
            updates.last(),
            Some(&PullUpdate {
                complete: true,
                ..progress(300, Some(300))
            })
        );
    }

    #[test]
    fn sends_completed_layers_despite_the_throttle() {
        let mut tracker = PullTracker::default();
        let start = Instant::now();
        tracker.update(&resolved(Some(300)), start);

        let first = tracker.update(&layer_progress(0, 50), start);
        let completed = tracker.update(&layer_complete(0, 300), start + PROGRESS_INTERVAL / 2);

        assert_eq!(first, Some(progress(50, Some(300))));
        assert_eq!(completed, Some(progress(300, Some(300))));
    }

    #[test]
    fn throttles_progress_but_not_completion() {
        let mut tracker = PullTracker::default();
        let start = Instant::now();
        tracker.update(&resolved(Some(300)), start);

        let first = tracker.update(&layer_progress(0, 50), start);
        let skipped = tracker.update(&layer_progress(0, 60), start + PROGRESS_INTERVAL / 2);
        let completed = tracker.update(&complete(), start + PROGRESS_INTERVAL / 2);

        assert_eq!(first, Some(progress(50, Some(300))));
        assert_eq!(skipped, None);
        assert_eq!(
            completed,
            Some(PullUpdate {
                complete: true,
                ..progress(60, Some(300))
            })
        );
    }
}
