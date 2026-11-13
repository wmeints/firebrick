# 62. Trust the Mozilla roots upstream on Windows hosts

## Status

Accepted

## Context

With TLS interception, microsandbox's proxy opens its own TLS connection to the
upstream server and verifies it against
`rustls_native_certs::load_native_certs()` plus the PEM files passed with
`TlsBuilder::upstream_ca_cert`. On Windows, `rustls_native_certs` reads only the
roots installed in the system store, and Windows installs most roots on demand
(Automatic Root Certificates Update), the first time CryptoAPI or schannel
builds a chain to them. rustls never triggers that. A sandbox with an enforced
network policy therefore failed with `unexpected eof` on every allowed HTTPS
host whose root Windows hadn't fetched yet, such as `example.com`, whose chain
ends at `SSL.com TLS ECC Root CA 2022`. The same host worked on Linux and macOS,
and on Windows once `curl.exe` on the host had made Windows fetch the root
(#265).

## Decision

On Windows hosts, `fbkd` writes the Mozilla roots of the `webpki-root-certs`
crate to `tls/upstream-roots.pem` in the microsandbox home whenever it creates a
sandbox with a network, rewriting the file when its content differs, and passes
it to `TlsBuilder::upstream_ca_cert`. Without `enforce` the file is recorded
with TLS interception off, so the interception that secrets turn on uses it too.
A failure to write the file fails the create with an error that names the path.
The `base64` crate encodes the roots as PEM. Both crates are dependencies of
`firebrick-daemon` for Windows targets only, in the versions `Cargo.lock`
already had through `rustls-platform-verifier`. Linux and macOS hosts keep
microsandbox's TLS settings unchanged.

## Consequences

- The proxy trusts the same public roots as the guest's own trust store,
  whichever roots Windows has fetched so far. The host's store, including
  corporate CAs, stays in use.
- The roots are as current as the `webpki-root-certs` version in the `fbkd`
  build; Dependabot's `Cargo.lock` updates keep them current.
- A sandbox reads the file each time it starts, but `fbkd` writes it only when
  it creates one, so a sandbox started after the file was removed verifies
  upstream servers against the host's store alone until it is recreated.
- Both dependencies can go once microsandbox verifies upstream servers with the
  platform verifier, such as `rustls-platform-verifier`, which asks CryptoAPI
  and so triggers the on-demand install.
