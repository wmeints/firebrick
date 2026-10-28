//! Limits how often a sandbox can make fbkd act on the host, such as opening a URL or setting
//! the host clipboard, so a loop in the guest can't flood the user.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Allows at most a number of events within a sliding window.
#[derive(Debug)]
pub(crate) struct RateLimit {
    max: usize,
    window: Duration,
    events: VecDeque<Instant>,
}

impl RateLimit {
    /// Returns a limit of `max` events within `window`.
    pub(crate) fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            events: VecDeque::new(),
        }
    }

    /// Records an event at `now` and returns `true` when the limit allows it. A refused event
    /// isn't recorded.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        while self
            .events
            .front()
            .is_some_and(|event| now.duration_since(*event) >= self.window)
        {
            self.events.pop_front();
        }

        if self.events.len() >= self.max {
            return false;
        }

        self.events.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_a_burst_then_waits_for_the_window() {
        let window = Duration::from_secs(10);
        let mut limit = RateLimit::new(3, window);
        let start = Instant::now();

        for _ in 0..3 {
            assert!(limit.allow(start));
        }
        assert!(!limit.allow(start + window / 2));
        assert!(limit.allow(start + window));
    }
}
