//! The firebrick CLI, `fbk`: starts and manages the sandbox of a workspace through the daemon,
//! keeps `.firebrick.yml` and the sandbox in sync, and connects the terminal to the sandbox.

mod approval;
pub mod client;
pub mod network;
pub mod port;
pub mod remote;
pub mod sandbox;
pub mod secret;
pub mod spec_file;
#[cfg(test)]
mod test_support;
mod ui;

/// Generated gRPC types for the firebrick API.
pub use firebrick_proto as api;
