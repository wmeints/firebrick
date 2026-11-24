# 68. Trust the interception CA in Docker containers through a runc wrapper

## Status

Accepted. Extends [ADR 0016](0016-run-docker-in-the-sandbox-vm.md).

## Context

A sandbox with TLS interception, from an enforced network policy or a secret,
trusts microsandbox's interception CA in its system bundle and, for Java, in the
trust store fbkd builds (see
[ADR 0018](0018-enforce-egress-with-microsandboxs-network-policy.md)). A
container that Docker or Testcontainers starts in the VM has its own files and
environment, but its HTTPS connections still leave through the VM's network and
are intercepted. Every HTTPS request in a container fails the certificate check,
for example when an integration test starts an upstream image that downloads
from Maven Central (issue #307). Users had to build Firebrick-specific variants
of such images, or mount the CA into each container by hand.

Docker has no daemon-wide setting that adds mounts or environment variables to
every container. It does let `daemon.json` name another OCI runtime as the
default, which `containerd-shim-runc-v2` calls instead of `runc` with the same
arguments, after it has written the container's `config.json` and mounted its
root filesystem.

## Decision

The base image installs `/usr/local/sbin/firebrick-runc`, a Python script, and
registers it as Docker's default runtime `firebrick` in
`/etc/docker/daemon.json`. It's always on; there's no setting.

On `create`, while `/.msb/tls/ca.pem` exists, which microsandbox writes only for
a sandbox with TLS interception, the script edits the container's `config.json`
before it runs `/usr/bin/runc` with the same arguments:

- It mounts the guest's `/etc/ssl/certs/ca-certificates.crt` read-only at
  `/etc/firebrick/ca-certificates.crt`, and the Java trust store at
  `/etc/firebrick/firebrick.p12` when fbkd has built it.
- It sets `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` and
  `NODE_EXTRA_CA_CERTS` to the mounted bundle when the container doesn't set
  them.
- A CA variable that the image or `docker run -e` sets keeps its value, because
  images such as `curlimages/curl` point it at their own bundle and other CAs in
  it, for example for TLS between containers, should keep working. The script
  writes a copy of that file with the CA appended into the container's bundle
  directory and mounts it read-only over the file. It resolves symlinks inside
  the container's root filesystem, as the container sees them, so it never reads
  a file of the VM. When the container has no such file, the variable points at
  the guest's bundle instead.
- It adds the trust store options to `JAVA_TOOL_OPTIONS`, after the container's
  own options, unless they set `-Djavax.net.ssl.trustStore=` themselves.

When the edit fails, the script writes the reason to stderr, which ends up in
`/var/log/dockerd.log`, and the container starts without the CA. All other runc
commands, and every command without TLS interception, pass through unchanged.

## Consequences

- Upstream images reach allowed hosts over HTTPS in a sandbox with TLS
  interception, without changes to the image or the test code. The network
  policy and TLS verification stay on; a container trusts no more than the VM
  already does.
- `docker exec` sessions get the mounts but not the variables, because Docker
  builds their environment from its own container config.
- `docker build` `RUN` steps don't go through the default runtime, so they don't
  trust the CA.
- Programs with a built-in CA store, or that read only a system bundle at a path
  the variables don't cover, still fail.
- Every container start runs a Python script in front of runc, also without TLS
  interception.
- A sandbox created from an older image gets the wrapper with `fbk upgrade`.
- Custom images that don't install the wrapper keep the old behavior.
