# 47. Share the host clipboard over a vsock route with host tools

## Status

Accepted

## Context

Programs in a sandbox can't read the host's clipboard. Text pastes through the
terminal, but tools that read the clipboard themselves, such as Claude Code's
image paste, run `wl-paste` or `xclip`, and the guest has no display server. A
shared clipboard is a way for data to cross the sandbox boundary, so it has to
be off by default and approved like `mounts` and `network`
([ADR 0037](0037-approve-mounts-and-network-changes-in-the-spec.md)), because
the agent can edit `.firebrick.yml`.

The guest needs a channel to `fbkd`, and `fbkd` needs a way to read the host
clipboard:

- **Channel.** The URL relay's exec stream
  ([ADR 0026](0026-relay-urls-to-the-host-through-an-exec-stream.md)) only
  carries lines from the guest to the host, and a reply would need a second
  channel. A vsock route
  ([ADR 0038](0038-record-network-activity-with-a-guest-logger-on-vsock.md)) is
  a stream in both directions, needs no network policy change and is fixed when
  the sandbox is created, so a sandbox without the setting can't get one from
  inside the guest.
- **Telling "off" from "unreachable".** A guest connection fails both to a vsock
  port without a route and to a route whose socket nobody listens on, so a
  failed connection doesn't tell the guest whether the sandbox lacks the setting
  or its `fbkd` is down. microsandbox writes the scripts of a sandbox's spec on
  the host and shows them in the guest at `/.msb/scripts/<name>` on every boot,
  also after a recreate.
- **Host clipboard.** A Rust crate such as `arboard` reads the clipboard
  in-process on Wayland, X11 and macOS, but returns images as decoded RGBA, so
  sending a PNG means encoding it again with another dependency, and its Wayland
  support needs the data-control protocol. The desktops' own tools, `wl-paste`
  (wl-clipboard), `xclip`, and `pbpaste` and `osascript` on macOS, return the
  bytes as they are on the clipboard, are what the stand-ins imitate, and add no
  code to `fbkd`. Either way `fbkd` needs the display of the user's desktop
  session. `fbk` starts `fbkd` as a child process, so it inherits
  `WAYLAND_DISPLAY` or `DISPLAY` from the first `fbk` the user runs.

## Decision

- **Opt-in setting.** `.firebrick.yml` gets a top-level `clipboard` boolean,
  `false` when absent. `fbk start` and `fbk run` add it to the approved settings
  in `approved.yml` and list `+ clipboard: shared with the host` or `-
  clipboard: shared with the host` when it differs. `approved.yml` leaves the
  field out while it's off, so older `fbk` versions can still read the file.
  `StartSandboxRequest` carries it as `optional bool clipboard`, applied when
  the sandbox is created.
- **Label, route and marker.** A sandbox created with the setting gets the
  `firebrick.clipboard=true` label. `SandboxSettings::builder` reads the label,
  so created, network-updated and upgraded sandboxes all get a stream route from
  guest vsock port `52001` to
  `$XDG_RUNTIME_DIR/firebrick/clipboard/<sandbox>.sock`, and the
  `firebrick-clipboard` script as a marker. On macOS without `XDG_RUNTIME_DIR`,
  the sockets are in `/private/tmp/fbkc-<uid>`, checked like the network log's
  directory
  ([ADR 0042](0042-put-the-network-log-sockets-in-a-checked-tmp-directory-on-macos.md));
  it sits next to that directory instead of inside it, so a socket of a
  63-character sandbox name still fits in 104 bytes. The sockets never fall back
  to a shared temp directory. Without the label there's neither route nor
  marker.
- **Protocol.** A stand-in sends one request line, `types`, `text` or `png`, and
  gets `ok <length>` and the content, or one of `empty`, `too-large`,
  `unavailable` and `invalid`. `fbkd` reads at most 16 bytes of the request,
  waits at most 5 seconds for it, accepts at most 4 connections per sandbox at a
  time and sends at most 1 MiB of text or 10 MiB of PNG.
- **Host tools.** `HostTools` reads the clipboard only when a stand-in asks: on
  macOS with `osascript -e 'clipboard info'`, `pbpaste` and `osascript` for
  `«class PNGf»`; on Linux with `wl-paste` when `WAYLAND_DISPLAY` is set and
  otherwise `xclip` when `DISPLAY` is. It runs them without a shell, stops
  reading their output past the limit, kills them after 4 seconds, and logs why
  the clipboard isn't available at `warn`, at most once a minute per sandbox.
  The `HostClipboard` trait lets the tests use a fake clipboard.
- **Stand-ins.** The `firebrick-base` image installs `image/firebrick-paste`, a
  Python script, as `/usr/local/bin/wl-paste` and `/usr/local/bin/xclip`. It
  supports the paste forms of both tools for text and PNG on the clipboard
  selection, uses the marker to say that sharing is off, and gives up after 5
  seconds.

## Consequences

- Programs in a sandbox with the setting can paste text and PNG images from the
  host clipboard, including Claude Code's image paste.
- Any process in such a sandbox can read the host clipboard whenever it wants,
  without the user noticing, including passwords the user copied. The setting is
  off by default and needs approval, and the user docs warn about this.
- The host needs `wl-clipboard` on Wayland or `xclip` on X11. Without them, or
  when `fbkd` was started without a display, for example from an SSH session,
  the stand-ins report that the host clipboard isn't available and `fbkd` logs
  why.
- Changing the setting takes `fbk rm --force && fbk start`, like `mounts`.
- Sandboxes on images older than the release that adds the stand-ins don't have
  them, and get the real tools' absence.
- Copying from the sandbox to the host can reuse the route, the protocol and the
  setting.
