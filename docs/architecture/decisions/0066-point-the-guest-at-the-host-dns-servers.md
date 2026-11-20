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
client can still go stale. Linux hosts whose `/etc/resolv.conf` lists the
network's servers have the same problem; hosts with systemd-resolved don't,
because `127.0.0.53` never changes. Windows hosts aren't affected: there the
forwarder asks the system's DNS client for every query.

The fix belongs in microsandbox, but it's not filed upstream yet, so `fbkd`
works around it. microsandbox intercepts every DNS query of the guest, not only
those to the gateway. A query to another server (its "direct path") gets a new
client per query and goes through the same domain rules, rebind protection and
resolved-address cache as a query to the gateway, as long as the network policy
allows port 53 to that server. When the policy denies it, the guest gets
NXDOMAIN, an answer, so glibc doesn't try the next server.

The options were:

- **Restart the sandboxes on wake.** Certain to work, but it kills every process
  in the sandbox, including the agent's sessions.
- **Point the guest at a fixed public resolver.** Simple, but it breaks VPN,
  corporate and split DNS.
- **Point the guest at the host's current DNS servers** and keep them current
  from `fbkd`.

## Decision

- **Allow DNS to any server.** Every sandbox with a network gets an egress rule
  that allows UDP and TCP port 53 to any address, and the `firebrick.dns=host`
  label. In an enforced policy the rule comes after the domain deny rules and
  before the IP and CIDR deny rules: a domain deny rule still makes its names
  NXDOMAIN, whichever server the guest asks, while an IP or CIDR deny rule that
  covers the host's DNS server, such as `192.168.0.0/16` on a home network,
  doesn't break resolution. A sandbox without enforcement gets microsandbox's
  default policy plus the rule instead of no policy. microsandbox answers all
  port-53 traffic itself, so the rule lets no other traffic out. It has a port,
  so it doesn't let private addresses through rebind protection.
- **Read the host's servers where the system keeps them.** On macOS, `fbkd`
  reads `ServerAddresses` of `State:/Network/Global/DNS` from the dynamic store,
  like microsandbox, because `/etc/resolv.conf` can miss or lag behind it with a
  VPN. It asks `/usr/sbin/scutil` instead of linking the `system-configuration`
  crate, whose dictionary lookups need `unsafe` code. Elsewhere it reads
  `/etc/resolv.conf`. It keeps at most two IPv4 servers that aren't loopback,
  link-local or unspecified.
- **Write them into the guest.** `fbkd` writes the servers into the guest's
  `/etc/resolv.conf` as root, followed by the gateway of the guest's default
  route, so the gateway stays the fallback and the only server when the host has
  none the guest can reach, such as systemd-resolved's `127.0.0.53`. The script
  only rewrites the file when it changes, and writes a new file that it moves
  into place, so the file is never half written.
- **Keep them current, and retry.** `fbkd` writes the servers when it starts a
  sandbox, through `fbk start` or `ssh`, and records them per sandbox. Every 5
  seconds it reads the host's servers again and writes them into each running
  sandbox with the label whose recorded servers differ, holding the sandbox's
  lock. A sandbox that fails, or whose lock is taken, such as one that is
  starting, is tried again at the next check; a recreated or restarted sandbox
  is forgotten, so it gets them again. When the host's servers can't be read,
  the sandboxes keep the servers they have. It does none of this on Windows
  hosts.

## Consequences

- DNS in a running sandbox recovers within about 5 seconds after the host's DNS
  servers change, without a restart.
- A sandbox can send DNS queries to any server, including servers on the host's
  LAN and ones the agent controls, even when a deny rule covers the address.
- Sandboxes created before this change have no rule, so `fbkd` leaves their DNS
  alone; they get it when they're recreated, for example by `fbk network allow`
  or `fbk upgrade`.
- The guest's `/etc/resolv.conf` is managed by `fbkd`; changes the user makes
  are overwritten when the host's servers change.
- Split DNS that macOS configures per domain, such as a VPN's internal zone,
  isn't in the global DNS servers, so the guest doesn't get it. The gateway path
  doesn't either.
- Once microsandbox refreshes its upstream servers, remove the workaround: drop
  the rule, the label and the sync, and bump microsandbox.
