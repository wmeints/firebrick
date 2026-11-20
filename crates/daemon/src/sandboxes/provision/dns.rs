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
use microsandbox::sandbox::SandboxSpec;
use microsandbox::{MicrosandboxError, Sandbox};
use microsandbox_network::policy::{Action, Destination, Direction, PortRange, Protocol, Rule};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use thiserror::Error;

/// Label of the sandboxes whose network policy lets the guest query the host's DNS servers.
pub const LABEL: &str = "firebrick.dns";

/// Value of [`LABEL`].
pub const FOLLOWS_HOST: &str = "host";

/// The most host DNS servers the guest gets. glibc reads three servers, and the last one is
/// the gateway.
const MAX_SERVERS: usize = 2;

/// How often fbkd checks whether the host's DNS servers changed.
const SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// How long writing the servers gets in the guest.
const APPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// The host's resolver configuration, which macOS keeps up to date with its primary DNS
/// servers.
const HOST_RESOLV_CONF: &str = "/etc/resolv.conf";

/// Writes `nameserver` lines for the arguments and then for the gateway of the default route
/// into `/etc/resolv.conf`, when that changes the file. `/proc/net/route` holds the gateway as
/// the hex digits of a little-endian IPv4 address. Uses only shell built-ins and `cat`.
const APPLY_SCRIPT: &str = r##"set -e
route=
while read -r _ destination gateway _; do
  if [ "$destination" = 00000000 ] && [ "$gateway" != 00000000 ]; then
    route=$gateway
    break
  fi
done < /proc/net/route
[ -n "$route" ] || { echo "the guest has no default route" >&2; exit 1; }
b1=${route#??????}
b2=${route#????}; b2=${b2%??}
b3=${route#??}; b3=${b3%????}
b4=${route%??????}
gateway=$(printf '%d.%d.%d.%d' "0x$b1" "0x$b2" "0x$b3" "0x$b4")
content="# Written by firebrick: the host's DNS servers, then the sandbox gateway."
for server in "$@"; do
  content="$content
nameserver $server"
done
content="$content
nameserver $gateway"
[ "$(cat /etc/resolv.conf 2>/dev/null)" = "$content" ] && exit 0
printf '%s\n' "$content" > /etc/resolv.conf
"##;

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

/// Returns the DNS servers of a `resolv.conf` that the guest can reach: at most
/// [`MAX_SERVERS`] IPv4 addresses that aren't loopback, link-local or unspecified. The guest
/// can't reach the host's loopback, such as systemd-resolved's `127.0.0.53`, and sandboxes from
/// `firebrick-base` have no IPv6.
pub fn nameservers(resolv_conf: &str) -> Vec<Ipv4Addr> {
    resolv_conf
        .lines()
        .filter_map(|line| line.strip_prefix("nameserver"))
        .filter_map(|rest| rest.split_whitespace().next()?.parse().ok())
        .filter_map(|address: IpAddr| match address {
            IpAddr::V4(address) => Some(address),
            IpAddr::V6(_) => None,
        })
        .filter(|address| {
            !address.is_loopback() && !address.is_link_local() && !address.is_unspecified()
        })
        .take(MAX_SERVERS)
        .collect()
}

/// Returns the host's DNS servers that the guest can reach, see [`nameservers`]. Empty when the
/// host's `resolv.conf` can't be read.
async fn host_nameservers() -> Vec<Ipv4Addr> {
    match tokio::fs::read_to_string(HOST_RESOLV_CONF).await {
        Ok(resolv_conf) => nameservers(&resolv_conf),
        Err(err) => {
            tracing::debug!("can't read the host's DNS servers from {HOST_RESOLV_CONF}: {err}");
            vec![]
        }
    }
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

/// Points the DNS of a sandbox that follows the host at the host's current servers, logging a
/// warning when that fails. Does nothing on Windows hosts.
pub async fn apply_host(sb: &Sandbox) {
    if cfg!(windows) || !follows_host(&sb.config().spec) {
        return;
    }

    apply_logged(sb, &host_nameservers().await).await;
}

/// Applies the servers, logging a warning when that fails.
async fn apply_logged(sb: &Sandbox, servers: &[Ipv4Addr]) {
    if let Err(err) = apply(sb, servers).await {
        tracing::warn!(
            "failed to point the DNS of sandbox {} at the host's DNS servers: {err}",
            sb.name()
        );
    }
}

/// Checks the host's DNS servers every [`SYNC_INTERVAL`], and points the running sandboxes
/// that follow the host at them at the first check and whenever they changed. Never returns,
/// except at once on Windows hosts.
pub async fn keep_in_sync() {
    if cfg!(windows) {
        return;
    }

    let mut ticks = tokio::time::interval(SYNC_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut applied = None;

    loop {
        ticks.tick().await;
        let servers = host_nameservers().await;

        if applied.as_ref() != Some(&servers) {
            apply_to_running(&servers).await;
            applied = Some(servers);
        }
    }
}

/// Points every running sandbox that follows the host at the servers.
async fn apply_to_running(servers: &[Ipv4Addr]) {
    let sandboxes = match crate::sandboxes::list_all().await {
        Ok(sandboxes) => sandboxes,
        Err(err) => {
            tracing::warn!("failed to list the sandboxes to update their DNS servers: {err}");
            return;
        }
    };

    for handle in sandboxes {
        let follows = handle
            .config()
            .is_ok_and(|config| follows_host(&config.spec));

        if !follows || handle.status_snapshot() != microsandbox::sandbox::SandboxStatus::Running {
            continue;
        }

        match handle.connect().await {
            Ok(sb) => apply_logged(&sb, servers).await,
            Err(err) => tracing::warn!(
                "failed to connect to sandbox {} to update its DNS servers: {err}",
                handle.name()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nameservers_keeps_the_first_two_reachable_ipv4_servers() {
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
            nameservers(resolv_conf),
            [Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(10, 0, 0, 53)]
        );
    }

    #[test]
    fn nameservers_is_empty_for_systemd_resolved() {
        assert!(nameservers("nameserver 127.0.0.53\noptions edns0 trust-ad\n").is_empty());
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
