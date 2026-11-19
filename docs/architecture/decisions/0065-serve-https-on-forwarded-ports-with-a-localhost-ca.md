# 65. Serve HTTPS on forwarded ports with a localhost CA

## Status

Accepted

## Context

Dev servers such as the Aspire dashboard serve HTTPS inside the sandbox with a
certificate they create there, such as the ASP.NET Core development certificate.
A browser on the host opens them through a port forward and warns about that
certificate. Making the host trust it would mean trusting a key that was created
in the guest, where the agent has root: the agent could read it and impersonate
`localhost` to the host, or swap in a certificate of its own.

The forward already sits between the browser and the guest. When `fbkd`
terminates the browser's TLS with a certificate whose key stays on the host, the
browser only has to trust the host's certificate, and the guest's certificate
doesn't matter.

What to trust has a few options:

- **A self-signed server certificate.** It can only identify `localhost`, but
  `update-ca-trust` on Arch and Fedora keeps only CA certificates in the bundles
  that browsers and OpenSSL read, and a new certificate needs trusting again.
- **A certificate authority**, as mkcert creates. Every trust store accepts it,
  and `fbkd` can issue new server certificates without asking the user again.
  Its key could sign a certificate for any name, so a name constraint limits it
  to `localhost`, `*.localhost`, `127.0.0.0/8` and `::1`, which browsers, NSS,
  OpenSSL and rustls enforce.

## Decision

- **A name-constrained localhost CA.** At startup `fbkd` loads the CA from
  `tls/ca.crt` and `tls/ca.key` in its data directory, or creates it there, in a
  directory and key file that only the user can read. It replaces the CA only
  when a file is missing or doesn't parse, or the certificate doesn't match the
  key, and logs a warning with the reason to run `fbk cert trust` again. Any
  other error, such as a clock before 1970 or a directory it can't write, stops
  `fbkd` instead, so it never throws away a CA the host trusts by mistake. The
  CA is valid for 20 years and isn't renewed, because rustls and most browsers
  don't check when a trusted CA expires, and reading the date back would take an
  X.509 parser.
- **A server certificate per start.** `fbkd` issues a certificate for
  `localhost`, `*.dev.localhost` (which Aspire uses), `127.0.0.1` and `::1`,
  valid for 825 days, the most macOS accepts, and keeps it in memory only.
- **Detect TLS per connection.** A configured forward waits for the first bytes
  of either side. A client that opens with a TLS handshake record (`0x16`) is
  accepted with the server certificate, and `fbkd` opens a TLS connection to the
  guest port that offers the client's ALPN protocols and its server name. The
  client's handshake then offers only the protocol the guest chose, so HTTP/1.1,
  HTTP/2, gRPC and WebSockets keep working, and the bytes are copied between the
  two TLS connections. Any other first bytes, a guest that speaks first and a
  client that closes get the plain relay. OAuth callback forwards stay plain.
- **Accept any guest certificate.** The guest is untrusted anyway, its dev
  certificates are issued by nobody, and the connection runs through the
  sandbox's SSH channel. `fbkd` still checks that the guest's server owns the
  key of its certificate.
- **`fbk cert trust` and `fbk cert clean`.** `fbk` changes the trust stores with
  the platform's tools, so it needs no code of its own for them, and replaces
  earlier firebrick CAs, which it finds by the common name `Firebrick localhost
  CA`:
  - Linux: `sudo install` into the first anchor directory that exists, then the
    distribution's update command, and `certutil` for the NSS databases of
    Chrome and of the Firefox profiles in `~/.mozilla/firefox`,
    `~/.config/mozilla/firefox`, the Snap and the Flatpak, when it's installed.
  - macOS: `security` with the login keychain.
  - Windows: `certutil -user` with the user's root store.

  A store's tool reports a missing certificate with a message of its own (`Could
  not find cert`, `could not be found`, `CRYPT_E_NOT_FOUND`); any other failure
  is an error, so `fbk cert clean` never reports a removal that didn't happen. A
  store that fails doesn't stop the others, so a refused `sudo` still updates
  Chrome and Firefox.
- **Dependencies.** `rcgen` creates the CA and the certificates, with the `time`
  crate for their dates. `rustls` and `tokio-rustls` were already in
  `Cargo.lock` through reqwest and become direct dependencies, with the same
  `aws-lc-rs` provider.

## Consequences

- An HTTPS dev server in a sandbox opens on the host without a warning after one
  `fbk cert trust`, also for forwards that existed before, and the keys that
  browsers trust never enter a sandbox.
- The CA's key lives in the user's data directory next to the SSH keys. Someone
  who can read it can impersonate `localhost` to this user's browsers, but no
  other name.
- A guest server that requires client certificates (mutual TLS) fails the
  handshake, because the browser's certificate ends at `fbkd`.
- A browser that connects with TLS to a guest that serves plain HTTP gets a
  closed connection, as it would get a protocol error before. `fbkd` logs the
  failed guest handshake at `info`.
- A client that trusts only the guest's own certificate, such as `curl --cacert`
  with the dev server's certificate or a database client with its own root, now
  gets the localhost certificate and has to trust the localhost CA instead.
- A guest server that only speaks TLS 1.0 or 1.1, or offers no cipher suite
  rustls supports, fails the handshake with `fbkd`, where the plain relay worked
  before. Forwards have no setting to turn the bridge off; one can be added if
  such servers turn up, since the forwards already take an optional bridge.
- Removing `fbkd`'s data directory creates a new CA at the next start, which
  needs `fbk cert trust` again.
- `fbk cert trust` changes the Linux system store with `sudo`, so it asks for
  the user's password, and Windows asks for confirmation.
