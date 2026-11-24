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
root filesystem. The BuildKit builder in `dockerd` doesn't use that runtime for
`docker build` steps: it runs `runc --log ... run --bundle <dir> --keep <id>`
itself, with the `runc` it finds on its `PATH`, and saves the step's root
filesystem as a layer once `runc run` returns.

## Decision

The base image installs `/usr/local/sbin/firebrick-runc`, a Python script, and
registers it as Docker's default runtime `firebrick` in
`/etc/docker/daemon.json`. It also links it as `/usr/local/sbin/runc`, which
comes before `/usr/bin/runc` on `dockerd`'s `PATH`, so BuildKit runs build steps
through it too. It's always on; there's no setting.

On `create` and `run`, while `/.msb/tls/ca.pem` exists, which microsandbox
writes only for a sandbox with TLS interception, the script edits the
container's `config.json` before it runs `/usr/bin/runc` with the same
arguments:

- It mounts a bundle read-only at `/etc/firebrick/ca-certificates.crt`: the
  image's own system bundle (`/etc/ssl/certs/ca-certificates.crt`,
  `/etc/pki/tls/certs/ca-bundle.crt` or `/etc/ssl/ca-bundle.pem`) with the CA
  appended, so CAs the image added with `update-ca-certificates` keep working,
  or the guest's system bundle when the image has none. It doesn't mount over
  the image's system bundle itself, so a container that rebuilds it still can.
- It sets `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` and
  `NODE_EXTRA_CA_CERTS` to that bundle when the container doesn't set them.
- A CA variable that the image or `docker run -e` sets keeps its value, because
  images such as `curlimages/curl` point it at their own bundle and other CAs in
  it, for example for TLS between containers, should keep working. The script
  writes a copy of that file with the CA appended into the container's bundle
  directory and mounts it read-only over the file. It reads the file from the
  bind mount or volume that holds it, when one does, and otherwise from the
  image, resolving symlinks inside the container's root filesystem as the
  container sees them, so it never reads a file of the VM the container doesn't
  have. When the container has no such file, the variable points at
  `/etc/firebrick/ca-certificates.crt` instead.
- It mounts the Java trust store at `/etc/firebrick/firebrick.p12` when fbkd has
  built it, and adds it to `JAVA_TOOL_OPTIONS`, after the container's own
  options, unless they set `-Djavax.net.ssl.trustStore=` themselves.

runc creates the files and directories it mounts at, such as `/etc/firebrick`,
in the root filesystem. For a foreground `run`, which BuildKit uses, the script
runs runc as a child process, passes on the signals it gets and runc's exit
code, and removes the empty mount points it caused before it returns, so they
don't end up in the step's layer. For `create` and a detached `run` it replaces
itself with runc.

When the edit fails for any reason, the script writes the reason to stderr,
which ends up in `/var/log/dockerd.log`, and the container starts without the
CA. All other runc commands, and every command without TLS interception, pass
through unchanged.

## Consequences

- Upstream images reach allowed hosts over HTTPS in a sandbox with TLS
  interception, without changes to the image or the test code. The network
  policy and TLS verification stay on; a container trusts no more than the VM
  already does.
- `docker exec` sessions get the mounts but not the variables, because Docker
  builds their environment from its own container config.
- `docker build` `RUN` steps trust the CA while they run, and the image keeps
  neither the mounts nor the variables. A builder of another driver, such as
  `docker buildx create --driver docker-container`, runs its own runc and
  doesn't.
- A container that Docker starts keeps the empty mount points in its writable
  layer, so `docker diff` lists `/etc/firebrick` and `docker commit` saves it.
- Programs with a built-in CA store, or that read only the image's system bundle
  and ignore the variables, still fail.
- Every container start runs a Python script in front of runc, also without TLS
  interception.
- A sandbox on an older default `firebrick-base` image gets the wrapper with
  `fbk upgrade`, which also removes its Docker images and containers. A sandbox
  whose spec pins an older `image` keeps that image and doesn't get it.
- A CA file in the image's system bundle path, read by a program that ignores
  the variables, doesn't get the CA, because mounting over it would break
  containers that rebuild it.
- Custom images that don't install the wrapper keep the old behavior.
