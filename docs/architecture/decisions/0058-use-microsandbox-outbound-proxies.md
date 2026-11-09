# Use microsandbox outbound proxies

Date: 2026-11-05

## Status

Proposed in the outbound proxy fork.

## Context

Proxy environment variables only affect cooperating tools. The locked
microsandbox 0.7.11 already provides host-side HTTP CONNECT, SOCKS4 and SOCKS5
transports through `SandboxBuilder::proxy`. Firebrick needs to expose that
capability without implementing a second proxy transport.

## Decision

Expose an optional `network.proxy` URL in `.firebrick.yml`, carry it over the
existing network RPC and validate it at both the CLI and daemon boundaries.
Accept literal IP addresses or ASCII DNS names with nonzero ports and no
credentials. Resolve names asynchronously using the host resolver when creating
or recreating the sandbox, with a five-second timeout. Prefer IPv4 and require
it for SOCKS4. Pass the selected numeric endpoint to microsandbox, while
retaining the hostname in settings review and the stored label. DNS failures
fail the operation and never select a direct transport. Running sandboxes retain
their selected address. Reuse microsandbox's proxy builder for transport,
including its behavior on connection failures and its existing policy and TLS
processing.

Include proxy additions, replacements and removals in settings review. Keep the
proxy in the stored network label even when enforcement or networking is off, so
network updates and image upgrades preserve the configuration. Labels without a
proxy retain their existing representation.

## Consequences

Tools need no proxy variables and cannot bypass the selected TCP proxy merely by
ignoring environment variables. This does not route DNS, image pulls or all
host-side activity through the proxy. HTTP CONNECT and SOCKS4 don't carry
non-DNS UDP; SOCKS5 can. Authentication, custom CA configuration, proxy address
failover and destination bypasses need separate follow-up work.

Tests cover config and RPC validation, settings review, label round trips and
the generated microsandbox config. A VM smoke test must additionally verify
HTTP, HTTPS, blocked destinations, proxy failure and TLS interception with the
actual external proxy before this is considered ready to merge.
