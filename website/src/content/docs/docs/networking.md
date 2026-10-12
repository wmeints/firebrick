---
title: Networking
description: Control where a sandbox may connect, take it offline, and forward ports to it.
---

By default a sandbox can reach the public internet, but not your private
networks. This page describes how to restrict it further, how to take it fully
offline, and how to reach servers in the sandbox from your host.

## Restricting outgoing traffic

To restrict the sandbox to the hosts you trust, turn on `enforce` in
`.firebrick.yml` and list them:

```yaml
name: my-project
network:
  enforce: true
  allow:
    - api.anthropic.com # exactly this host name
    - "*.github.com" # github.com and every subdomain
    - 140.82.112.4 # one IPv4 or IPv6 address
    - 192.168.10.0/24 # a CIDR range
  deny:
    - gist.github.com
```

With `enforce: true`, the sandbox can only connect to destinations an `allow`
rule matches: TCP and UDP on any port, and ICMP. A `deny` rule wins over an
`allow` rule, so in the example `gist.github.com` is blocked although
`*.github.com` allows it. Every name resolves, except names a domain `deny` rule
matches. `fbk validate` and `fbk start` reject entries that aren't a host name,
`*.` plus a domain, an IP address or a CIDR range, such as `*`,
`https://github.com` or `github.com:443`. With `enforce: false`, the rules are
only validated.

An HTTP or HTTPS request to a host that isn't allowed gets `403 Forbidden` with
a message that names the host:

```sh
$ curl -s https://example.org
firebrick blocked the connection to example.org: the network policy of this sandbox doesn't allow it. To allow it, run `fbk network allow example.org` on the host, outside the sandbox.
```

Keep in mind that:

- Only HTTP/1.x requests get the message, and only while there are no domain
  `deny` rules. With a domain `deny` rule, and for IP or CIDR `deny` rules and
  other protocols, the connection is reset or closed instead, and a host denied
  by a domain rule doesn't resolve.
- A host allowed by a host name or `*.domain` rule is only reachable over HTTP
  and HTTPS on ports 80 and 443. For SSH to `github.com:22` or HTTPS on another
  port, allow its IP address or CIDR range.
- Firebrick intercepts HTTPS to check the host name, with a certificate
  authority that the sandbox trusts. Tools that pin certificates or bring their
  own CA store fail. HTTP/3 is blocked, so clients fall back to HTTP/2 or 1.1.
- A secret's allowed hosts must be allowed by the network rules too. See
  [Secrets](/docs/secrets/).
- `mise install` downloads its tools when the sandbox starts, so allow the hosts
  it needs, or set `mise: false`.

## Changing the rules with `fbk network`

Change the rules with `fbk network` in the project directory. It writes the
change to `.firebrick.yml`, so the file and the sandbox always have the same
rules, and applies it to the sandbox:

```sh
$ fbk network allow example.org "*.npmjs.org"
updated the network rules of my-project
$ fbk network deny gist.github.com
updated the network rules of my-project
$ fbk network policy enable
updated the network rules of my-project
```

- `fbk network allow <rule>...` adds the rules to `allow` and removes them from
  `deny`; `fbk network deny <rule>...` does the opposite. A rule that's already
  there isn't added twice. When neither the file nor the sandbox changes, the
  command prints `network rules are already up to date`.
- `fbk network policy enable` and `fbk network policy disable` set `enforce`.
- When applying the rules to the sandbox fails, run the command again: it
  applies the rules from `.firebrick.yml` to a sandbox that doesn't have them
  yet, also after you edit the file by hand.
- When there's no `.firebrick.yml` yet, the command creates one with the
  defaults and the name `fbk start` uses, plus the change.
- `allow` and `deny` warn when `enforce` is off, because the rules don't protect
  anything until you run `fbk network policy enable`.
- When the sandbox doesn't exist yet, only the file changes and the rules apply
  when it starts.
- The sandbox keeps its files, installed packages and Docker data, but it
  restarts: running processes stop, as after `fbk stop` and `fbk start`. A
  stopped sandbox stays stopped. A sandbox that already has the rules, or whose
  rules aren't enforced, doesn't restart. A paused sandbox has to be resumed
  first.
- `fbk network` rewrites `.firebrick.yml`, which drops its comments and
  formatting.
- `fbk network` sends the whole network section of `.firebrick.yml`, so it asks
  before it sends changes you didn't make with the command, such as `enforce:
  false` that the agent added by hand:

  ```console
  $ fbk network allow example.org
  .firebrick.yml changes settings you haven't approved for this workspace:
    ~ network.enforce: true -> false
    + network.allow: example.org
  Apply these changes? [y/N]
  ```

  Answer `y` to apply them, or anything else to leave the sandbox alone; the
  change to `.firebrick.yml` stays. `--yes` approves them without asking. The
  change you asked for never needs approval. See
  [Approving mounts and network changes](/docs/configuration/#approving-mounts-and-network-changes).

## Reaching hosts on your local network

A sandbox can't reach a server on your LAN, such as a NAS at `192.168.68.122`,
even when your host can. This is by design: by default a sandbox only reaches
the public internet, and private address ranges are blocked, whether or not
`enforce` is on. They include the RFC 1918 ranges `10.0.0.0/8`, `172.16.0.0/12`
and `192.168.0.0/16`, IPv6 unique local addresses (`fc00::/7`) and carrier-grade
NAT (`100.64.0.0/10`).

To reach a LAN host, turn on the policy and allow the host's IP address, or a
CIDR range for the whole LAN:

```sh
$ fbk network policy enable
updated the network rules of my-project
$ fbk network allow 192.168.68.122     # or 192.168.68.0/24 for the whole LAN
updated the network rules of my-project
```

Or the same in `.firebrick.yml`:

```yaml
name: my-project
network:
  enforce: true
  allow:
    - 192.168.68.122
```

Enabling the policy means the sandbox only reaches what the `allow` rules list,
so also allow the public hosts the sandbox still needs, such as your agent's API
and package registries. See
[Restricting outgoing traffic](#restricting-outgoing-traffic).

A host name that resolves to a LAN address, such as `nas.example.lan` in your
internal DNS, needs an IP or CIDR rule for that address. A host-name rule alone
isn't enough: to protect against DNS rebinding, the sandbox's resolver answers
`NXDOMAIN` to any lookup whose answer contains a private address that no IP or
CIDR rule allows. With the rule for the address, the name resolves and the
sandbox connects to it over any protocol, HTTPS included, so it needs no
host-name rule of its own.

When a LAN host isn't allowed, you see one of these:

- Without `enforce`, or for other protocols than HTTP/1.x, the connection is
  refused or reset: `curl: (7) Failed to connect to 192.168.68.122 port 80`.
- With `enforce` on, an HTTP/1.x request gets `403 Forbidden` with the message
  `firebrick blocked the connection to 192.168.68.122`.
- A host name that resolves to a LAN address without an IP or CIDR rule doesn't
  resolve: `curl: (6) Could not resolve host: nas.example.lan`.

## Recording network activity

Firebrick records the domains and IP addresses each sandbox contacts, so you can
see which `fbk network allow` rules a task needs, or spot an agent that contacts
hosts it shouldn't. A logger in the `firebrick-base` image watches the sandbox's
network interface and reports each DNS lookup with the addresses it resolved to,
and each outgoing TCP connection attempt and UDP destination, to `fbkd`.
Attempts that the enforced rules deny are recorded too.

`fbkd` keeps one entry per domain and per address, with how often and when it
first and last saw it, in `~/.local/state/firebrick/network/<sandbox>.json`.
`fbk rm` deletes the record with the sandbox.

Run `fbk network log` in the project directory to see the record of its sandbox,
newest first:

```sh
$ fbk network log
┌──────────────────┬──────┬───────┬───────┬─────────────────────┬─────────┐
│ DESTINATION      │ KIND │ PORTS │ COUNT │ LAST SEEN           │ POLICY  │
├──────────────────┼──────┼───────┼───────┼─────────────────────┼─────────┤
│ evil.example.com │ dns  │       │ 1     │ 2026-10-11 14:03:40 │ denied  │
│ api.github.com   │ dns  │       │ 12    │ 2026-10-11 14:02:11 │ allowed │
│ 140.82.112.6     │ tcp  │ 443   │ 12    │ 2026-10-11 14:02:11 │ allowed │
└──────────────────┴──────┴───────┴───────┴─────────────────────┴─────────┘
```

`POLICY` tells whether the sandbox's current rules allow the destination: domain
rules for `dns` entries, IP and CIDR rules for `tcp` and `udp` entries. A
connection to the address of an allowed domain shows as `denied` unless an IP
rule allows the address too, and every entry shows `allowed` when the rules
aren't enforced. It's Firebrick's own reading of the rules, not a report of what
was blocked, and rules you changed since apply to older entries too. The command
only reads: it doesn't change `.firebrick.yml` and never asks for approval.

The logger runs inside the sandbox, and `agent` can use `sudo`, so an agent can
stop it or report made-up traffic, and `fbk network log` shows what it reported.
Treat the record as a diagnostic aid, not as proof of what the sandbox did. DNS
over HTTPS and IPv6 aren't recorded. Sandboxes created by an older version of
Firebrick have no record until you remove and start them again, or a `fbk
network` command that changes their settings restarts them; `fbk network log`
tells you so. Custom images only have a record when they run the logger (see
[Custom images](/docs/custom-images/)).

## Working offline

To keep an agent fully offline, for example for code that must not leave your
machine, remove the sandbox's network device:

```yaml
name: my-project
network:
  enabled: false # default: true
```

Or run `fbk network disable` in the project directory, and `fbk network enable`
to turn the network back on:

```sh
$ fbk network disable
disabled the network of my-project
$ fbk run -- curl -sI https://github.com
curl: (6) Could not resolve host: github.com
$ fbk network enable
enabled the network of my-project
```

Without a network device, nothing in the sandbox resolves or connects, and
`enforce`, `allow` and `deny` are validated but ignored. `fbk run`, `ssh
<leaf>.fbk`, port forwards and the editor integrations keep working, because
they don't use the sandbox's network. `mise install` can't download tools, so
install them while the network is on, or set `mise: false`.

This is different from `fbk network policy disable`: that keeps the network
device and only stops enforcing the rules, so the sandbox can reach the internet
again. `fbk network disable` takes the network away entirely, whatever the rules
say.

The commands work like the other `fbk network` commands: they create
`.firebrick.yml` when it's missing, print `the network is already disabled` or
`the network is already enabled` when nothing changes, restart an existing
sandbox while keeping its disks, and only change the file when the sandbox
doesn't exist yet (`updated .firebrick.yml; the change applies when my-project
starts`).

## Forwarding ports

The sandbox publishes no ports on the host. To open a server in the sandbox,
such as a dev server, from the browser on your host, list its port under
`ports`, written like Docker Compose:

```yaml
name: my-project
ports:
  - 3000 # host localhost:3000 -> sandbox port 3000
  - "8080:5173" # host localhost:8080 -> sandbox port 5173
```

`fbk start` prints each forward:

```text
Forwarding localhost:3000 -> sandbox port 3000
Forwarding localhost:8080 -> sandbox port 5173
```

The forwards reach `127.0.0.1` in the sandbox, so a server that only listens on
`localhost` there works. They listen on `localhost` on the host only, never on
your network. To change them, edit the list and run `fbk start` again; the
sandbox keeps running and unchanged forwards keep their connections. `fbk stop`
and `fbk rm` close them, and an SSH connection that starts the sandbox opens
them again.

To add or remove a forward while you work, without editing the file:

```sh
fbk port forward 3000          # host localhost:3000 -> sandbox port 3000
fbk port forward 8080:5173     # host localhost:8080 -> sandbox port 5173
fbk port rm 8080               # stop forwarding host port 8080
```

They apply right away to a running sandbox and update the `ports` list in
`.firebrick.yml`, keeping its comments and other fields, or create the file when
there's none. Forwarding a host port that's already listed replaces its sandbox
port. For a stopped sandbox, or one that doesn't exist yet, the change applies
when it starts. When the host port is in use, `fbk port forward` fails with
`couldn't forward localhost:<port>: <reason>` and changes nothing.

When a host port is already in use, the sandbox still starts and `fbk start`
prints `warning: couldn't forward localhost:<port>: <reason>`. Free the port and
run `fbk start` again. Ports must be from 1 to 65535, and each host port may
appear once.

### Browser logins

Logins that send your browser back to `localhost`, such as `gh auth login
--web`, need no listed port. The sandbox has no browser, so the base image hands
URLs it opens to `fbkd`, which opens them in the browser on your host. When such
a URL's `redirect_uri` or own host is `localhost:<port>`, `fbkd` forwards that
port to the sandbox before the browser opens. The forward closes after 10
minutes without connections, or when the sandbox stops.

## SSH

SSH doesn't use the sandbox's network either. The generated SSH config tunnels
each connection through `fbk` and the daemon's socket to the SSH server in the
sandbox; no sshd port is published on the host. See
[Editor support](/docs/editor-support/).

## IPv6

The base image disables IPv6 in the guest. microsandbox gives the guest IPv6
even when the host can't route it, and then resets IPv6 connections after the
handshake, so clients never fall back to IPv4
([microsandbox#1226](https://github.com/superradcompany/microsandbox/issues/1226)).
The image's `/sbin/init` applies the setting at boot, so images built on the
base image inherit it, unless they set `init: false`.
