//! Connects to a running sandbox through the daemon: `fbk run` attaches the terminal to a
//! process in the sandbox, and `fbk ssh-proxy` carries an SSH connection to it.

pub mod session;
pub mod ssh;
