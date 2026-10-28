//! Applies the parts of a sandbox's spec to the sandbox: its environment variables, the host's
//! git identity, the workspace's mise tools, the image pull and the network policy.

pub mod env;
pub(crate) mod git;
pub mod mise;
pub(crate) mod network;
pub(crate) mod pull;
