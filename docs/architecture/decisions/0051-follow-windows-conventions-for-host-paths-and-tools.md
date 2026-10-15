# 51. Follow Windows conventions for host paths and tools

## Status

Accepted

## Context

Besides its transport ([ADR 0050](0050-talk-over-named-pipes-on-windows.md)),
firebrick relied on Unix conventions in several places:

- **Directories.** The data, state and config directories fell back to
  `$HOME/.local/share`, `$HOME/.local/state` and `$HOME/.config`
  ([ADR 0023](0023-keep-a-firebrick-microsandbox-home-in-the-xdg-state-directory.md)).
  Windows has no `HOME`, but Git Bash sets one, so `fbk` would find different
  daemons from PowerShell and from Git Bash.
- **Canonical paths.** On Windows `fs::canonicalize` returns verbatim paths such
  as `\\?\C:\Users\me\project`, and microsandbox refuses those as bind mount
  hosts. Windows paths also keep the case they were typed in, so the same
  directory could get two sandbox names.
- **Host programs.** URLs opened with `xdg-open` or `open`, and the clipboard
  used `wl-paste`, `xclip` or `pbpaste`.
- **The terminal.** `fbk run` resized the guest's terminal on `SIGWINCH` and
  wrote the guest's bytes to stdout. The Windows console has neither the signal
  nor VT input by default, and it refuses bytes that aren't UTF-8.
- **Unix modes.** Private files and directories got mode `0600` or `0700`.

## Decision

On Windows:

- An absolute XDG variable still wins. Otherwise the data and state directories
  are `%LOCALAPPDATA%\firebrick\data` and `%LOCALAPPDATA%\firebrick\state` (the
  microsandbox home is `state\msb`), and the approvals are in
  `%APPDATA%\firebrick\approved.yml`. `HOME` is ignored. `~` in mounts and
  `~/.ssh/config` come from `USERPROFILE`, and `~user` is an error. The editors'
  settings are under `%APPDATA%`.
- Where Unix sets mode `0700` or `0600`, firebrick gives the directory or file a
  protected access list that grants access to the user, `LocalSystem` and the
  administrators only. That covers the SSH directory, the microsandbox home and
  its database, and every file `write_private` writes: the secrets, the
  approvals and the network records. The files created in such a directory, such
  as the SSH keys, inherit its list. This doesn't rely on the profile's own
  list, which an `XDG_*` variable or `MSB_HOME` outside the profile would lose,
  and it's what Win32-OpenSSH requires of a private key. Setting the list takes
  `SetNamedSecurityInfoW` in `firebrick_utils::win32`
  ([ADR 0050](0050-talk-over-named-pipes-on-windows.md)).
- Before `fbk` starts `fbkd`, it stops its own stdin, stdout and stderr from
  being inherited. On Windows a child inherits every inheritable handle of its
  parent, so `fbkd` would otherwise keep the pipes `fbk` was started with open,
  and an `ssh` whose `ProxyCommand` started the daemon, or an `fbk ls | more`,
  would wait for it.
- `fbk` and `fbkd` canonicalize with `dunce::canonicalize`, which removes the
  `\\?\` prefix where that's safe and is `fs::canonicalize` on Unix. `dunce` was
  already in `Cargo.lock`. `fbk` canonicalizes its working directory on Windows
  only, so sandbox names don't change on Unix.
- URLs open with `rundll32 url.dll,FileProtocolHandler <url>`. A shell such as
  `cmd /c start` would run the `&` and `^` of a query string as commands.
- The clipboard is read and written by short `powershell.exe -NoProfile` scripts
  that use the .NET clipboard and pass raw bytes on stdin and stdout. Windows
  PowerShell comes with every Windows and runs in the single-threaded apartment
  the clipboard needs. The programs `fbkd` runs get `CREATE_NO_WINDOW`, because
  `fbkd` has no console.
- `fbk run` polls the terminal size every 250 ms. It turns on VT input and
  output with `crossterm_winapi`, which crossterm already uses, and writes a
  byte the console refuses as `U+FFFD`.
- The generated SSH config uses forward slashes, which Win32-OpenSSH accepts and
  which aren't escape characters inside its quotes.

## Consequences

- One `fbkd` per user on Windows, whichever shell starts `fbk`.
- `~user` mounts don't work on Windows.
- The clipboard costs a PowerShell start per request, a few hundred
  milliseconds, and a test can't stub it without replacing the developer's
  clipboard, so the unit tests only check the commands.
- The tests that create symlinks or check modes run on Unix only, because
  creating a symlink on Windows takes administrator rights or Developer Mode.
