# 18. Enforce egress with microsandbox's network policy

## Status

Accepted

## Context

Users must be able to control the egress network traffic of a sandbox (see
[Introduction and goals](../01-introduction-and-goals.md)). Without any
configuration, a sandbox gets microsandbox's default policy: the public internet
is allowed and private networks are blocked. A coding agent can then reach any
public host, including ones the user doesn't trust.

The solution strategy plans a proxy with TLS interception and policies that
control which traffic is allowed. microsandbox already runs such a proxy on the
host for every sandbox: it evaluates an ordered, first-match list of rules for
each outgoing connection, reads the host name of HTTPS connections from the TLS
SNI, can intercept TLS with its own CA, and can answer a denied HTTP request
with a `403 Forbidden` page. The rules are fixed when the sandbox is created.

## Decision

`.firebrick.yml` gets an optional `network` section with `enforce`, `allow` and
`deny`. Each rule is a host name, `*.` plus a domain, an IP address or a CIDR
range. `firebrick-spec` parses the rules, so `fbk validate`, `fbk start` and
`fbkd` reject the same entries.

With `enforce: true`, `fbkd` creates the sandbox with a microsandbox network
policy instead of building its own proxy:

1. All `deny` rules, so a deny rule wins over an allow rule.
2. All `allow` rules.
3. microsandbox's DNS rule (`Rule::allow_dns()`), so names that no domain rule
   denies resolve.
4. Deny for any other egress.

It turns on TLS interception for port 443 and microsandbox's HTTP deny response,
with a message that names the host and the `fbk network allow` command. With
`enforce: false` or no `network` section, the sandbox gets microsandbox's
default policy and no TLS interception, as before.

The daemon builds the policy in its `network` module with the
`microsandbox-network` crate, at the version `microsandbox` pins, because
`microsandbox` doesn't re-export the rule builder and destination types.

## Consequences

- The rules apply to all egress: TCP and UDP on any port, and ICMP. HTTPS is
  matched on the SNI.
- microsandbox only answers HTTP/1.x requests to hosts that no rule matched with
  the deny page. As soon as the policy has a domain `deny` rule, every denied
  HTTP(S) request gets a TCP reset instead, because microsandbox treats the
  deferred deny rule as an explicit deny when the connection opens. A host
  denied by a domain rule doesn't resolve at all, because microsandbox also
  matches domain rules against DNS queries. IP and CIDR denies and non-HTTP
  traffic get a reset or a close.
- microsandbox's strict mode stays on: a host allowed only by a domain rule
  can't be reached with protocols other than HTTP(S) on ports 80 and 443, such
  as SSH to `github.com:22`. Those need an IP or CIDR rule.
- TLS interception adds the microsandbox CA to the guest's trust store. Clients
  with their own CA store or certificate pinning fail, as they already do when a
  secret is set ([ADR 0004](0004-store-secrets-in-a-private-file.md)). Java,
  whose JDKs only read their own `cacerts`, is the exception: fbkd builds a
  PKCS12 trust store from the system bundle on every start and points every JVM
  at it with `JAVA_TOOL_OPTIONS` (see
  [Trusting the interception CA in Java](../06-runtime-view.md#trusting-the-interception-ca-in-java)).
  QUIC (UDP/443) is dropped and clients fall back to TCP.
- A secret's allowed hosts don't open the network: with enforcement on, they
  must also be allowed by the network policy.
- Changing the rules means recreating the sandbox, because microsandbox can't
  change the policy of an existing sandbox.
- microsandbox doesn't report denied connections to the host, so `fbkd` can't
  show them to the user.
