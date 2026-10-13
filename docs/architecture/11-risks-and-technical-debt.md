# Risks and technical debt

- **No native Windows support:** the constraints require the application to work
  on Windows, but the CLI and daemon talk over a unix socket and rely on unix
  signals, so they don't compile for Windows and no Windows release exists
  ([ADR 0001](decisions/0001-release-through-github-releases-and-ghcr.md)).
  Porting needs a cross-platform transport, such as named pipes, and
  microsandbox's Windows support is still in preview. Until then, Windows users
  run the Linux release in WSL 2 with nested virtualization, and editors on
  Windows can't reach the sandboxes' SSH hosts.
- **Secrets in plaintext at rest:** the secret values are stored in plaintext in
  `secrets.yml` and in microsandbox's database, readable by every process that
  runs as the user. When the host may be compromised, rotate the secrets at
  their issuers ([ADR 0004](decisions/0004-store-secrets-in-a-private-file.md)).
- **Secrets only work in HTTP headers:** Firebrick keeps microsandbox's default
  substitution scope, which replaces secret placeholders in headers, including
  decoded Basic auth credentials, but not in URLs or request bodies. Tools that
  send a token elsewhere can't use secrets.
- **Unreleased builds have no default image:** the default image is the
  `firebrick-base` image tagged with the workspace version, which only exists
  after that version is released. Development builds need an `image` in
  `.firebrick.yml`
  ([ADR 0005](decisions/0005-default-to-the-anvil-base-image-of-the-same-release.md)).
- **No IPv6 in the default image:** `firebrick-base` disables guest IPv6 to work
  around a microsandbox bug that resets IPv6 connections on hosts without IPv6
  egress. Images with `init: false` keep guest IPv6 and the bug, so downloads
  from servers with IPv6 addresses fail there on such hosts. The hint for images
  without `/sbin/init` relies on microsandbox's error text. Remove the
  workaround once microsandbox fixes
  [issue 1226](https://github.com/superradcompany/microsandbox/issues/1226)
  ([ADR 0007](decisions/0007-disable-guest-ipv6-in-the-base-image.md)).
- **Nothing supervises `dockerd`:** the base image's init starts `dockerd` in
  the background and doesn't wait for it, so commands that run right after the
  sandbox boots may find it not ready yet, and nothing restarts it when it dies.
  Agents can read `/var/log/dockerd.log` and start it again with `sudo sh -c
  'dockerd >/var/log/dockerd.log 2>&1 &'`
  ([ADR 0016](decisions/0016-run-docker-in-the-sandbox-vm.md)).
- **Workspace ownership is only tested with host UID 1000:** the integration
  tests check that the workspace shows up as `1000:1000` and that UID 1000 can
  write to it, but they run on hosts where the user has UID 1000. Mapping
  another host UID, such as `501` on macOS, relies on microsandbox's mount owner
  override ([ADR 0006](decisions/0006-run-sandboxes-as-the-agent-user.md)).
- **No SSH agent forwarding:** microsandbox's SSH server rejects agent
  forwarding, so git over SSH needs a private key inside the sandbox. Git uses
  HTTPS with a secret instead.
- **Limits of egress enforcement:** microsandbox serves the deny page only to
  HTTP/1.x requests to hosts that no rule matched, and only while the policy has
  no domain `deny` rule. With a domain `deny` rule, denied requests get a TCP
  reset, and the hosts that rule denies don't resolve. IP and CIDR denies and
  non-HTTP traffic get a reset or a close. Strict mode blocks protocols other
  than HTTP(S) on ports 80 and 443, such as SSH, to hosts allowed only by a
  domain rule; they need an IP or CIDR rule. TLS interception breaks clients
  that pin certificates or bring their own CA store. microsandbox has no API for
  denied connections; the network log records the attempts the guest makes, but
  not which ones the policy denied
  ([ADR 0018](decisions/0018-enforce-egress-with-microsandboxs-network-policy.md)).
- **The agent can tamper with the network record:** the logger that reports a
  sandbox's DNS lookups and connections runs inside the VM, and the `agent` user
  has passwordless `sudo`, so an agent can stop it, hide its traffic from it or
  send false events. `fbkd` treats every event as untrusted input and bounds the
  record, but the record stays a diagnostic aid, not an audit trail. DNS over
  TCP, DNS over HTTPS and IPv6 aren't recorded. Replace the logger once
  microsandbox reports DNS queries and connections itself
  ([ADR 0038](decisions/0038-record-network-activity-with-a-guest-logger-on-vsock.md)).
- **Forwarded ports listen on the host:** `fbkd` listens on the loopback ports
  in `.firebrick.yml`, so any process of any user on the host can connect to
  them and reach the sandbox's servers behind them, like a dev server running on
  the host. They only listen on `127.0.0.1` and `::1`, never on other addresses.
  A port that another process uses is skipped with a warning, so the sandbox's
  server isn't reachable there until the port is free and `fbk start` runs again
  ([ADR 0019](decisions/0019-forward-ports-through-the-ssh-servers-direct-tcpip.md)).
- **Sandboxes can open URLs on the host:** any process in a sandbox can make the
  host browser open an http or https URL without asking. `fbkd` refuses hosts on
  the host's local network and, for sandboxes with enforced egress rules, hosts
  the rules don't allow, but it checks the URL's host name, not the addresses it
  resolves to: a public name that resolves to a private address still opens, and
  data can still leave through an allowed host, such as a query string to
  `github.com`. The browser sends the user's cookies with the request
  ([ADR 0026](decisions/0026-relay-urls-to-the-host-through-an-exec-stream.md)).
- **Sandboxes can make the host listen on loopback ports:** a sandbox that opens
  a URL with a loopback `redirect_uri` or host and an explicit port makes `fbkd`
  listen on that port of the host's `127.0.0.1` and `::1` for up to 10 minutes
  without connections. Any process of any user on the host can connect to it and
  reach the sandbox, and a sandbox can take a free port before a host service
  that wants it starts. The rate limit of 5 URLs per 10 seconds bounds how fast
  it can claim ports. When the port is busy, a URL with that `redirect_uri`
  still opens, so the browser sends the OAuth callback, with its code, to
  whatever listens on the host's port
  ([ADR 0030](decisions/0030-forward-oauth-callback-ports-when-opening-a-url.md)).
- **Only mounts, network and clipboard changes need approval:** the agent can
  edit `.firebrick.yml`, and `fbk` only asks before it applies changed `mounts`,
  `network` or `clipboard` settings. A changed `image`, `init`, `mise`,
  `resources`, `volumes`, `ports` or `name` applies the next time the sandbox is
  created, or for `ports` the next time it starts, without asking. `--yes`
  approves whatever the file contains. A workspace from before approvals existed
  has no entry, and its first sandbox creation with the default network section
  doesn't ask: when the agent removes the `network` section and the user runs
  `fbk rm --force && fbk start`, the new sandbox doesn't enforce the old rules.
  `fbk` can't compare with the removed sandbox, and `GetSandbox` doesn't return
  a running sandbox's policy
  ([ADR 0037](decisions/0037-approve-mounts-and-network-changes-in-the-spec.md)).
- **A shared clipboard crosses the sandbox boundary:** in a sandbox created with
  `clipboard: true`, any process can read the host clipboard through `fbkd` at
  any time, without the user noticing, including passwords and tokens the user
  copied for something else. `fbkd` reads it only when asked and caps each
  answer at 1 MiB of text or 10 MiB of PNG, but doesn't ask the user before each
  paste. The setting is off by default and needs approval, and the route only
  exists in sandboxes created with it
  ([ADR 0047](decisions/0047-share-the-host-clipboard-over-a-vsock-route-with-host-tools.md)).
- **Workspace files can run code on the host:** the workspace is mounted
  read/write, so the agent can plant files that run on the host when the user
  works on the project there, such as git hooks, `.git/config` settings like
  `core.fsmonitor`, editor tasks, `.envrc` files and mise configs. Firebrick
  doesn't detect or block them; the user documentation lists the common ones and
  tells users to review the workspace, including `.git/`, before they use it on
  the host
  ([ADR 0017](decisions/0017-trust-the-workspaces-mise-config-inside-the-sandbox.md)).
- **Ageing Linux build runners:** Linux releases build on the `ubuntu-22.04`
  runners to support glibc 2.35. GitHub retires runner images before their
  Ubuntu release reaches end of support (April 2027), after which the release
  jobs fail. Building in an older-glibc container, or with `cargo-zigbuild`
  against a pinned glibc version, would remove this dependency.
