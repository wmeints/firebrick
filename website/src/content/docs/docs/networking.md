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
  [Secrets](/firebrick/docs/secrets/).
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
[Editor support](/firebrick/docs/editor-support/).

## IPv6

The base image disables IPv6 in the guest. microsandbox gives the guest IPv6
even when the host can't route it, and then resets IPv6 connections after the
handshake, so clients never fall back to IPv4
([microsandbox#1226](https://github.com/superradcompany/microsandbox/issues/1226)).
The image's `/sbin/init` applies the setting at boot, so images built on the
base image inherit it, unless they set `init: false`.
