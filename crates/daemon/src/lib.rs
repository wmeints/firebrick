//! The firebrick daemon, `fbkd`: manages microsandbox sandboxes for coding agents and serves
//! the `SandboxManagementService` gRPC API that `fbk` talks to.

/// Generated gRPC types for the firebrick API.
pub use firebrick_proto as api;

pub mod forward;
pub mod guest;
mod host_config;
pub mod runtime;
pub mod sandboxes;
pub mod secrets;
pub mod server;
pub mod ssh;
