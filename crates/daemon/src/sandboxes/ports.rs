//! The port forwards of sandboxes: the mappings stored on each sandbox, and keeping the open
//! forwards in line with them.

use super::SandboxManager;
use super::error::SandboxError;
use super::status::{get_sandbox, list_all_sandboxes};
use crate::forward::{self, ForwardReport};
use firebrick_spec::{PortMapping, with_port};
use microsandbox::Sandbox;
use microsandbox::sandbox::{SandboxHandle, SandboxStatus};

impl SandboxManager {
    /// Opens the forwards of the sandboxes that are running, from their stored ports. Used
    /// when the daemon starts.
    pub async fn restore_forwards(&self) {
        let sandboxes = match list_all_sandboxes().await {
            Ok(sandboxes) => sandboxes,
            Err(err) => {
                tracing::warn!("failed to restore port forwards: {err}");
                return;
            }
        };
        let _ports = self.ports_lock.lock().await;

        for sb in sandboxes
            .iter()
            .filter(|sb| sb.status_snapshot() == SandboxStatus::Running)
        {
            self.forwards.apply(sb.name(), &stored_ports(sb)).await;
        }
    }
    /// Stores the ports with the sandbox when they're given, then makes its open forwards
    /// match the stored ports.
    pub(super) async fn open_forwards(
        &self,
        name: &str,
        ports: Option<&[PortMapping]>,
    ) -> ForwardReport {
        let _ports = self.ports_lock.lock().await;
        let ports = match (ports, Sandbox::get(name).await) {
            (Some(ports), Ok(sb)) => {
                store_ports(&sb, ports).await;
                ports.to_vec()
            }
            (Some(ports), Err(_)) => ports.to_vec(),
            (None, Ok(sb)) => stored_ports(&sb),
            (None, Err(_)) => vec![],
        };

        self.forwards.apply(name, &ports).await
    }
    /// Adds the forward to the ports stored with the sandbox, replacing a stored one with the
    /// same host port, and opens it when the sandbox runs. Returns whether the sandbox runs.
    /// Fails with `FailedPrecondition` when the host port can't be listened on, and then keeps
    /// the stored ports and the open forwards.
    pub async fn forward_port(&self, name: &str, port: PortMapping) -> Result<bool, SandboxError> {
        let _guard = self.lock_sandbox(name).await;
        let _ports = self.ports_lock.lock().await;
        let sb = get_sandbox(name).await?;
        let stored = stored_ports(&sb);
        let change = PortsChange {
            ports: with_port(&stored, port),
            stored,
            requested: Some(port),
        };

        self.change_ports(&sb, &change).await
    }
    /// Removes the forward of the host port from the ports stored with the sandbox, and closes
    /// it when the sandbox runs. Returns whether the sandbox runs. Fails with `NotFound` when
    /// the sandbox doesn't forward the host port.
    pub async fn remove_port(&self, name: &str, host: u16) -> Result<bool, SandboxError> {
        let _guard = self.lock_sandbox(name).await;
        let _ports = self.ports_lock.lock().await;
        let sb = get_sandbox(name).await?;
        let stored = stored_ports(&sb);
        let ports: Vec<PortMapping> = stored.iter().filter(|p| p.host != host).copied().collect();

        if ports.len() == stored.len() {
            return Err(SandboxError::not_found(format!(
                "port {host} isn't forwarded for sandbox {name}"
            )));
        }

        let change = PortsChange {
            stored,
            ports,
            requested: None,
        };

        self.change_ports(&sb, &change).await
    }
    /// Makes the open forwards of a running sandbox match the changed ports, then stores them
    /// with the sandbox. Returns whether the sandbox runs. When a new forward can't be opened or
    /// the ports can't be stored, reopens the stored forwards and fails.
    async fn change_ports(
        &self,
        sb: &SandboxHandle,
        change: &PortsChange,
    ) -> Result<bool, SandboxError> {
        let running = sb.status_snapshot() == SandboxStatus::Running;

        if running {
            self.apply_new_ports(sb.name(), change).await?;
        }

        let saved = save_ports(sb, &change.ports).await;

        if saved.is_err() && running {
            self.forwards.apply(sb.name(), &change.stored).await;
        }

        saved.map(|()| running)
    }
    /// Makes the open forwards of the sandbox match the changed ports. Fails with
    /// `FailedPrecondition` when a new forward can't be opened, after reopening the stored
    /// forwards.
    async fn apply_new_ports(&self, name: &str, change: &PortsChange) -> Result<(), SandboxError> {
        let report = self.forwards.apply(name, &change.ports).await;
        let Some(failure) = report.failed.iter().find(|f| change.is_new(f.port)) else {
            return Ok(());
        };

        self.forwards.apply(name, &change.stored).await;

        Err(SandboxError::failed_precondition(failure.reason.clone()))
    }
}

/// A change to the ports stored with a sandbox.
struct PortsChange {
    /// The ports stored before the change.
    stored: Vec<PortMapping>,
    /// The ports after the change.
    ports: Vec<PortMapping>,
    /// The forward the change asks for, if any.
    requested: Option<PortMapping>,
}

impl PortsChange {
    /// Whether the change opens the forward: it's the requested one, which must open even when
    /// it's stored already, or it isn't stored. A stored forward that already failed to open
    /// doesn't block other changes.
    fn is_new(&self, port: PortMapping) -> bool {
        self.requested == Some(port) || !self.stored.contains(&port)
    }
}

/// Returns the ports stored with the sandbox.
fn stored_ports(sb: &SandboxHandle) -> Vec<PortMapping> {
    sb.config()
        .ok()
        .and_then(|config| config.spec.labels.get(forward::PORTS_LABEL).cloned())
        .map(|value| forward::ports_from_label(&value))
        .unwrap_or_default()
}

/// Stores the ports with the sandbox like [`save_ports`], logging a warning when that fails.
async fn store_ports(sb: &SandboxHandle, ports: &[PortMapping]) {
    if let Err(err) = save_ports(sb, ports).await {
        tracing::warn!("{err}");
    }
}

/// Stores the ports with the sandbox when they differ from the stored ones. The label doesn't
/// affect the running VM, so it's applied on the next start rather than restarting the sandbox.
async fn save_ports(sb: &SandboxHandle, ports: &[PortMapping]) -> Result<(), SandboxError> {
    if stored_ports(sb) == ports {
        return Ok(());
    }

    sb.modify()
        .label(forward::PORTS_LABEL, forward::label_value(ports))
        .next_start()
        .apply()
        .await
        .map_err(|err| {
            tracing::error!(error = ?err, "failed to store the ports of sandbox {}", sb.name());
            SandboxError::internal(format!(
                "failed to store the ports of sandbox {}",
                sb.name()
            ))
        })?;

    Ok(())
}
