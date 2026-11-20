//! Keeps the guest's DNS servers in step with the host's, as a workaround for a microsandbox
//! forwarder that reads the host's servers only when the VM starts (issue #293).
//!
//! The guest's `/etc/resolv.conf` points at the sandbox gateway, where microsandbox forwards
//! queries to the DNS servers the host had when the sandbox started. After the host wakes from
//! sleep or changes networks those servers can be gone, and name resolution in the guest fails
//! until the sandbox restarts. microsandbox intercepts every DNS query of the guest, not only
//! the ones to the gateway: a query to another server goes there over a fresh connection,
//! after the same domain rules, rebind protection and network log as a query to the gateway.
//! So fbkd writes the host's current DNS servers into the guest's `/etc/resolv.conf`, followed
//! by the gateway, and writes them again when they change.
//!
//! A query to another server also needs the network policy to allow port 53 to that server,
//! which [`rule`] does. Sandboxes created with that rule carry [`LABEL`], so fbkd leaves the
//! DNS servers of older sandboxes alone. Windows hosts don't need any of this: there microsandbox
//! asks the system's DNS client for every query.

use super::{ExitError, check_exit};
use microsandbox::sandbox::{SandboxHandle, SandboxSpec, SandboxStatus};
use microsandbox::{MicrosandboxError, Sandbox};
use microsandbox_network::policy::{Action, Destination, Direction, PortRange, Protocol, Rule};
use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use thiserror::Error;

/// Label of the sandboxes whose network policy lets the guest query the host's DNS servers.
pub const LABEL: &str = "firebrick.dns";

/// Value of [`LABEL`].
pub const FOLLOWS_HOST: &str = "host";

/// The most host DNS servers the guest gets. glibc reads three servers, and the last one is
/// the gateway.
const MAX_SERVERS: usize = 2;

/// How often fbkd checks the host's DNS servers and retries the sandboxes it couldn't update.
const SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// How long writing the servers gets in the guest.
const APPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Writes `nameserver` lines for the arguments and then for the gateway of the default route
/// into `/etc/resolv.conf`, when that changes the file.
const APPLY_SCRIPT: &str = include_str!("firebrick-dns.sh");

/// Errors of writing the DNS servers into a sandbox.
#[derive(Error, Debug)]
pub enum ApplyError {
    /// The script couldn't run, for example because the image has no shell.
    #[error(transparent)]
    Exec(#[from] MicrosandboxError),
    /// The script exited non-zero, with the reason on stderr.
    #[error("writing /etc/resolv.conf {0}")]
    Exited(#[from] ExitError),
}

/// Returns the egress rule that lets the guest send DNS queries to any server. microsandbox
/// answers every query on port 53 itself, after the domain rules, so the rule lets no other
/// traffic out.
pub fn rule() -> Rule {
    Rule {
        direction: Direction::Egress,
        destination: Destination::Any,
        protocols: vec![Protocol::Udp, Protocol::Tcp],
        ports: vec![PortRange::single(53)],
        action: Action::Allow,
    }
}

/// Whether the sandbox with the spec was created with [`rule`].
pub fn follows_host(spec: &SandboxSpec) -> bool {
    spec.labels.get(LABEL).map(String::as_str) == Some(FOLLOWS_HOST)
}

/// Returns the servers of the `nameserver` lines of a `resolv.conf` that the guest can reach,
/// see [`reachable`].
#[cfg(any(not(target_os = "macos"), test))]
pub fn resolv_conf_servers(resolv_conf: &str) -> Vec<Ipv4Addr> {
    reachable(
        resolv_conf
            .lines()
            .filter_map(|line| line.strip_prefix("nameserver"))
            .filter_map(|rest| rest.split_whitespace().next()),
    )
}

/// Returns the servers of the `ServerAddresses` array in the output of `scutil` for
/// `show State:/Network/Global/DNS` that the guest can reach, see [`reachable`]. The array's
/// entries are lines such as `0 : 192.168.1.1`.
#[cfg(any(target_os = "macos", test))]
pub fn scutil_servers(output: &str) -> Vec<Ipv4Addr> {
    let entries = output
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("ServerAddresses :"))
        .skip(1)
        .take_while(|line| line.trim() != "}")
        .filter_map(|line| Some(line.split_once(" : ")?.1.trim()));

    reachable(entries)
}

/// Returns at most [`MAX_SERVERS`] of the addresses that are IPv4 and not loopback, link-local
/// or unspecified. The guest can't reach the host's loopback, such as systemd-resolved's
/// `127.0.0.53`, and sandboxes from `firebrick-base` have no IPv6, so other addresses, such as
/// IPv6 ones, are skipped.
fn reachable<'a>(addresses: impl Iterator<Item = &'a str>) -> Vec<Ipv4Addr> {
    addresses
        .filter_map(|address| address.parse::<Ipv4Addr>().ok())
        .filter(|address| {
            !address.is_loopback() && !address.is_link_local() && !address.is_unspecified()
        })
        .take(MAX_SERVERS)
        .collect()
}

/// Reads the host's DNS servers that the guest can reach from `/etc/resolv.conf`.
#[cfg(not(target_os = "macos"))]
async fn host_servers() -> std::io::Result<Vec<Ipv4Addr>> {
    let resolv_conf = tokio::fs::read_to_string("/etc/resolv.conf").await?;

    Ok(resolv_conf_servers(&resolv_conf))
}

/// Reads the host's primary DNS servers that the guest can reach from the dynamic store, where
/// macOS keeps them. `/etc/resolv.conf` can miss or lag behind them with a VPN. Asks `scutil`,
/// which reads the store without the `unsafe` calls of the SystemConfiguration bindings.
#[cfg(target_os = "macos")]
async fn host_servers() -> std::io::Result<Vec<Ipv4Addr>> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new("/usr/sbin/scutil")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(b"show State:/Network/Global/DNS\n").await?;
    }

    let output = tokio::time::timeout(APPLY_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| std::io::Error::other("scutil didn't answer in time"))??;

    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "scutil exited with {}",
            output.status
        )));
    }

    Ok(scutil_servers(&String::from_utf8_lossy(&output.stdout)))
}

/// Points the guest's DNS at the servers, followed by the sandbox gateway, so names resolve
/// through the gateway alone when there are none.
pub async fn apply(sb: &Sandbox, servers: &[Ipv4Addr]) -> Result<(), ApplyError> {
    let mut args = vec![
        "-c".to_string(),
        APPLY_SCRIPT.to_string(),
        "firebrick-dns".to_string(),
    ];
    args.extend(servers.iter().map(ToString::to_string));

    let output = sb
        .exec_with("sh", |exec| {
            exec.args(args).user("root").cwd("/").timeout(APPLY_TIMEOUT)
        })
        .await?;

    Ok(check_exit(&output)?)
}

/// The DNS servers fbkd last wrote into each running sandbox that follows the host.
#[derive(Default)]
pub struct Applied(Mutex<HashMap<String, Vec<Ipv4Addr>>>);

impl Applied {
    /// Whether the sandbox doesn't have the servers yet, as far as fbkd knows.
    pub fn needs(&self, name: &str, servers: &[Ipv4Addr]) -> bool {
        self.map().get(name).map(Vec::as_slice) != Some(servers)
    }

    /// Records that the sandbox has the servers.
    pub fn record(&self, name: &str, servers: &[Ipv4Addr]) {
        self.map().insert(name.to_string(), servers.to_vec());
    }

    /// Forgets the servers of the sandbox, so the next check writes them again.
    pub fn forget(&self, name: &str) {
        self.map().remove(name);
    }

    /// Forgets the sandboxes that aren't in `running`, so a sandbox that restarts gets the
    /// servers again.
    pub fn retain(&self, running: &HashSet<String>) {
        self.map().retain(|name, _| running.contains(name));
    }

    /// Returns the map. It stays consistent when a thread panics while holding it, so a
    /// poisoned lock is safe to use.
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Ipv4Addr>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Points the DNS of a sandbox that follows the host at the host's current servers and records
/// them, logging a warning when that fails. Leaves the sandbox alone when the host's servers
/// can't be read, and does nothing on Windows hosts.
pub async fn apply_host(sb: &Sandbox, applied: &Applied) {
    if cfg!(windows) || !follows_host(&sb.config().spec) {
        return;
    }

    applied.forget(sb.name());

    match host_servers().await {
        Ok(servers) => apply_and_record(sb, &servers, applied).await,
        Err(err) => tracing::warn!(
            "can't read the host's DNS servers, so sandbox {} keeps the ones it has: {err}",
            sb.name()
        ),
    }
}

/// Applies the servers and records them, or logs a warning when that fails.
async fn apply_and_record(sb: &Sandbox, servers: &[Ipv4Addr], applied: &Applied) {
    match apply(sb, servers).await {
        Ok(()) => applied.record(sb.name(), servers),
        Err(err) => tracing::warn!(
            "failed to point the DNS of sandbox {} at the host's DNS servers: {err}",
            sb.name()
        ),
    }
}

/// Every [`SYNC_INTERVAL`], reads the host's DNS servers and writes them into the running
/// sandboxes that follow the host and don't have them yet. A sandbox that fails or whose lock
/// `try_lock` can't take now is tried again at the next check. Never returns, except at once on
/// Windows hosts.
pub async fn keep_in_sync<G>(applied: &Applied, try_lock: impl Fn(&str) -> Option<G>) {
    if cfg!(windows) {
        return;
    }

    let mut ticks = tokio::time::interval(SYNC_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut host = HostServers::default();

    loop {
        ticks.tick().await;

        if let Some(servers) = host.read().await {
            apply_to_running(&servers, applied, &try_lock).await;
        }
    }
}

/// Reads the host's DNS servers and logs when what it reads changes.
#[derive(Default)]
struct HostServers {
    last: Option<Result<Vec<Ipv4Addr>, String>>,
}

impl HostServers {
    /// Returns the host's DNS servers, or `None` when they can't be read.
    async fn read(&mut self) -> Option<Vec<Ipv4Addr>> {
        let result = host_servers().await.map_err(|err| err.to_string());

        if self.last.as_ref() != Some(&result) {
            log_host_servers(&result);
        }

        self.last = Some(result.clone());
        result.ok()
    }
}

/// Logs the host's DNS servers that the sandboxes follow from now on.
fn log_host_servers(result: &Result<Vec<Ipv4Addr>, String>) {
    match result {
        Ok(servers) if servers.is_empty() => tracing::info!(
            "the host has no DNS server the sandboxes can reach, so they use the sandbox gateway \
             only"
        ),
        Ok(servers) => tracing::info!("sandboxes follow the host's DNS servers {servers:?}"),
        Err(err) => tracing::warn!(
            "can't read the host's DNS servers, so the sandboxes keep the ones they have: {err}"
        ),
    }
}

/// Writes the servers into every running sandbox that follows the host and doesn't have them
/// yet, holding the sandbox's lock.
async fn apply_to_running<G>(
    servers: &[Ipv4Addr],
    applied: &Applied,
    try_lock: impl Fn(&str) -> Option<G>,
) {
    let sandboxes = match crate::sandboxes::list_all().await {
        Ok(sandboxes) => sandboxes,
        Err(err) => {
            tracing::warn!("failed to list the sandboxes to update their DNS servers: {err}");
            return;
        }
    };

    let running: Vec<SandboxHandle> = sandboxes.into_iter().filter(follows_and_runs).collect();
    applied.retain(&running.iter().map(|sb| sb.name().to_string()).collect());

    for handle in running {
        if !applied.needs(handle.name(), servers) {
            continue;
        }

        // A sandbox that is busy, such as one that is starting, is tried at the next check.
        let Some(_guard) = try_lock(handle.name()) else {
            continue;
        };

        match handle.connect().await {
            Ok(sb) => apply_and_record(&sb, servers, applied).await,
            Err(err) => tracing::warn!(
                "failed to connect to sandbox {} to update its DNS servers: {err}",
                handle.name()
            ),
        }
    }
}

/// Whether the sandbox runs and follows the host's DNS servers. Logs a warning when its config
/// can't be read.
fn follows_and_runs(handle: &SandboxHandle) -> bool {
    if handle.status_snapshot() != SandboxStatus::Running {
        return false;
    }

    match handle.config() {
        Ok(config) => follows_host(&config.spec),
        Err(err) => {
            tracing::warn!(
                "failed to read the config of sandbox {} to update its DNS servers: {err}",
                handle.name()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolv_conf_servers_keeps_the_first_two_reachable_ipv4_servers() {
        let resolv_conf = "# macOS\n\
             search lan\n\
             nameserver 127.0.0.53\n\
             nameserver fe80::1%en0\n\
             nameserver 2001:db8::53\n\
             nameserver 169.254.1.1\n\
             nameserver 192.168.1.1 # router\n\
             nameserver\t10.0.0.53\n\
             nameserver 1.1.1.1\n\
             nameserver not-an-address\n";

        assert_eq!(
            resolv_conf_servers(resolv_conf),
            [Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(10, 0, 0, 53)]
        );
    }

    #[test]
    fn resolv_conf_servers_is_empty_for_systemd_resolved() {
        assert!(resolv_conf_servers("nameserver 127.0.0.53\noptions edns0 trust-ad\n").is_empty());
    }

    #[test]
    fn scutil_servers_reads_only_the_server_addresses() {
        let output = "<dictionary> {\n\
             \x20 SearchDomains : <array> {\n\
             \x20   0 : lan\n\
             \x20 }\n\
             \x20 ServerAddresses : <array> {\n\
             \x20   0 : fe80::1%en0\n\
             \x20   1 : 10.8.0.1\n\
             \x20   2 : 192.168.1.1\n\
             \x20 }\n\
             \x20 __CONFIGURATION_ID__ : Default: 0\n\
             }\n";

        assert_eq!(
            scutil_servers(output),
            [Ipv4Addr::new(10, 8, 0, 1), Ipv4Addr::new(192, 168, 1, 1)]
        );
    }

    #[test]
    fn scutil_servers_is_empty_without_the_key() {
        assert!(scutil_servers("  No such key\n").is_empty());
    }

    #[test]
    fn applied_needs_the_servers_until_they_are_recorded_and_after_a_restart() {
        let applied = Applied::default();
        let old = [Ipv4Addr::new(10, 0, 0, 1)];
        let new = [Ipv4Addr::new(10, 0, 0, 2)];

        assert!(applied.needs("a", &old));
        applied.record("a", &old);
        assert!(!applied.needs("a", &old));
        assert!(applied.needs("a", &new));

        applied.retain(&HashSet::new());
        assert!(applied.needs("a", &old));

        applied.record("a", &old);
        applied.forget("a");
        assert!(applied.needs("a", &old));
    }

    #[test]
    fn rule_allows_dns_to_any_server_and_nothing_else() {
        let rule = rule();

        assert_eq!(rule.direction, Direction::Egress);
        assert_eq!(rule.action, Action::Allow);
        assert!(matches!(rule.destination, Destination::Any));
        assert_eq!(rule.ports, [PortRange::single(53)]);
        assert_eq!(rule.protocols, [Protocol::Udp, Protocol::Tcp]);
    }
}
