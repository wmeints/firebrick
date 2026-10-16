//! The `fbkd` daemon that manages Firebrick microVM sandboxes for coding agents.

/// Runs the `fbkd` daemon until it shuts down.
fn main() -> anyhow::Result<()> {
    firebrick_daemon::app::main()
}
