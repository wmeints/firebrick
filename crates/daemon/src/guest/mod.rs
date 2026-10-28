//! Channels a sandbox uses to make fbkd act on the host: sharing the clipboard, recording the
//! network activity and opening URLs in the host's browser. Each sandbox gets its own channel
//! endpoint, and a rate limit keeps a loop in the guest from flooding the user.

pub mod clipboard;
pub mod netlog;
pub mod open;
mod rate_limit;
mod vsock;

pub(crate) use vsock::panicked;
#[cfg(test)]
pub(crate) use vsock::test_endpoint_dir;
