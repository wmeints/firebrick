# 66. Point the guest at the host's DNS servers

## Status

Accepted

## Context

After a macOS host wakes from sleep, DNS lookups in a running sandbox fail until
the sandbox restarts (issue #293). The guest's `/etc/resolv.conf` points at the
sandbox gateway, where microsandbox 0.7.15's forwarder sends queries to the
host's DNS servers. It reads those servers once, when the VM starts
(`read_host_dns_servers` in `engine/dns/forwarder.rs`), and never refreshes
them, so a VPN that reconnects or a resolver that changes during sleep leaves
the forwarder on servers that no longer answer. Its UDP client opens a new
socket for every query, so a dead connection isn't the cause; its cached TCP
client can still go stale. Windows hosts aren't affected: there the forwarder
asks the system's DNS client for every query.

The fix belongs in microsandbox, but it's not filed upstream yet, so `fbkd`
works around it. microsandbox intercepts every DNS query of the guest, not only
those to the gateway. A query to another server (its "direct path") gets a new
client per query and goes through the same domain rules, rebind protection and
resolved-address cache as a query to the gateway, as long as the network policy
allows port 53 to that server.

The options were:

- **Restart the sandboxes on wake.** Certain to work, but it kills every process
  in the sandbox, including the agent's sessions.
- **Point the guest at a fixed public resolver.** Simple, but it breaks VPN,
  corporate and split DNS.
- **Point the guest at the host's current DNS servers** and keep them current
  from `fbkd`.

## Decision

- **Allow DNS to any server.** Every sandbox with a network gets an egress rule
  that allows UDP and TCP port 53 to any address, after the other rules, and the
  `firebrick.dns=host` label. A sandbox without enforcement gets microsandbox's
  default policy plus that rule instead of no policy. microsandbox answers all
  port-53 traffic itself after the domain rules, so the rule lets no other
  traffic out, and a host denied by a domain rule still doesn't resolve. The
  rule has a port, so it doesn't let private addresses through rebind
  protection.
- **Write the host's servers into the guest.** `fbkd` reads the host's
  `/etc/resolv.conf`, which macOS keeps in step with its primary DNS servers,
  and keeps at most two IPv4 servers that aren't loopback, link-local or
  unspecified. It writes them into the guest's `/etc/resolv.conf` as root,
  followed by the gateway of the guest's default route, so the gateway stays the
  fallback and the only server when the host has none the guest can reach, such
  as systemd-resolved's `127.0.0.53`. The script only rewrites the file when it
  changes.
- **Keep them current.** `fbkd` writes the servers when it starts a sandbox, and
  checks the host's servers every 5 seconds, writing them into every running
  sandbox with the label when they changed and once when `fbkd` starts. It does
  none of this on Windows hosts.

## Consequences

- DNS in a running sandbox recovers within about 5 seconds after the host's DNS
  servers change, without a restart.
- Sandboxes created before this change have no rule, so `fbkd` leaves their DNS
  alone; they get it when they're recreated, for example by `fbk network allow`
  or `fbk upgrade`.
- The guest's `/etc/resolv.conf` is managed by `fbkd`; changes the user makes
  are overwritten when the host's servers change.
- Split DNS that macOS configures per domain, such as a VPN's internal zone,
  isn't in `/etc/resolv.conf`, so the guest doesn't get it. The gateway path
  doesn't either.
- Once microsandbox refreshes its upstream servers, remove the workaround: drop
  the rule, the label and the sync, and bump microsandbox.
