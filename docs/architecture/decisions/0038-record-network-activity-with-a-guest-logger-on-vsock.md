# 38. Record network activity with a guest logger on vsock

## Status

Accepted

## Context

Users can't see which domains and IP addresses an agent in a sandbox tries to
reach. They can't tell which `fbk network allow` rules a task needs, or spot an
agent that contacts hosts it shouldn't. microsandbox enforces the network policy
on the host, but has no API that reports a sandbox's DNS queries or connections,
denied or not.

What the host can do:

- Read the guest's traffic from the outside. microsandbox's network stack runs
  inside the `msb` process and offers no hook for it, and capturing there would
  mean changing microsandbox.
- Run a process in the guest that watches the traffic and reports it. That
  process needs a channel to `fbkd`. The URL relay's exec stream (ADR 0026) only
  runs while a client connects through `fbkd`, and the network policy can't
  safely expose a host service. A vsock route can: `SandboxBuilder::vsock`
  connects a guest vsock port on host CID 2 to a host unix socket, without
  touching the network policy. Like the policy, it's fixed at create time (ADR
  0021, ADR 0026).

The guest kernel of microsandbox 0.7.8 (Linux 6.12) supports `AF_PACKET` and
`AF_VSOCK`. microsandbox connects to the host socket each time the guest
connects, so the socket doesn't have to exist when the VM boots, but it checks
the path when the VM starts, so a path that doesn't fit in a unix socket address
stops the sandbox from booting.

## Decision

- **Capture with `tcpdump` in the guest.** The `firebrick-base` image installs
  `tcpdump` and `python3` from Ubuntu's apt repositories, and its `/sbin/init`
  starts `/usr/local/sbin/firebrick-netlog` as root, restarting it when it
  exits. The script runs two `tcpdump`s on the interface of the default route:
  outgoing UDP datagrams and TCP SYNs, and incoming answers from port 53. It
  pairs each `A?` query with its answer and reports every TCP SYN and the first
  UDP datagram to each new destination. A packet capture sees attempts the
  policy denies and connections that end at once, because the packets leave the
  guest either way. We chose it over `conntrack -E`, which needs
  `nf_conntrack_netlink` and doesn't see DNS answers. The BPF filters keep bulk
  TCP traffic out of the script. Python, rather than a shell pipeline, lets the
  logger pair queries with answers and send without blocking.
- **Report over vsock.** The logger writes one JSON object per line to vsock
  port `52000` of the host. It never blocks the capture: it drops events while
  `fbkd` can't be reached or its 64 KiB buffer is full, and reconnects with a
  backoff of at most 5 seconds.
- **One route and socket per sandbox.** `SandboxSettings::builder` adds a stream
  route from port `52000` to `$XDG_RUNTIME_DIR/firebrick/netlog/<sandbox>.sock`,
  so created and recreated sandboxes get it. The socket name tells `fbkd` which
  sandbox an event comes from without trusting the guest to say so. When the
  path is too long for a unix socket, or `XDG_RUNTIME_DIR` isn't set, the
  sandbox gets no route. The sockets never fall back to the shared temp
  directory, where another user could take them over. On macOS, which has no
  `XDG_RUNTIME_DIR`, they're in a checked `/private/tmp/fbk-<uid>` instead
  ([ADR 0042](0042-put-the-network-log-sockets-in-a-checked-tmp-directory-on-macos.md)).
- **Treat events as untrusted input.** The agent has root through `sudo`, so it
  can stop the logger or send false events. `fbkd` checks each line, ignores the
  guest's timestamp, accepts at most 4 connections per sandbox, and bounds the
  record to 10,000 destinations and 64 addresses or ports per destination.
  Network logging never stops a sandbox from starting.
- **Aggregate per destination.** `fbkd` keeps one entry per DNS name and one per
  address and protocol, with counts and first and last seen, in
  `$XDG_STATE_HOME/firebrick/network/<sandbox>.json`, written privately at most
  once a second. A follow-up issue shows the record with `fbk network log`.
- **Dependencies.** `fbkd` depends on `serde_json` for the events and the record
  and on `chrono` for the timestamps. Both were already in `Cargo.lock` through
  microsandbox, so no new third-party code is added.

## Consequences

- Users get a per-sandbox record of the domains and IP addresses its guest
  contacted, including attempts the enforced policy denied.
- The record is a diagnostic aid, not an audit trail: an agent with root can
  stop the logger, hide traffic from it or make up events.
- DNS over TCP, DNS over HTTPS, AAAA records and IPv6 destinations aren't
  recorded. IPv6 is disabled in the guest (ADR 0007).
- The base image grows by `python3` and `tcpdump`, and each sandbox runs a
  Python process and two `tcpdump`s.
- Sandboxes created before this change have no route until an `fbk network`
  command recreates them. Images without the logger get a route that nothing
  uses.
- Events sent while `fbkd` is down are lost.
- This is meant to be replaced once microsandbox reports a sandbox's DNS queries
  and connections itself. The logger and the route can then go away, and the
  record format can stay.
