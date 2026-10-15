---
name: smoke-test
description: "Run the fbk CLI and fbkd daemon end to end in an isolated environment, against real microsandbox VMs, without touching the user's daemon, sandboxes or config. Use to try a change by hand, reproduce a bug, check `fbk start`/`run`/`ssh` behavior, or boot a locally built sandbox image."
---

# Smoke test

Try `fbk` and `fbkd` for real in a throwaway environment. The user usually has
their own `fbkd` running, sandboxes in `~/.local/state/firebrick/msb` (and older
ones or their own `msb` in `~/.microsandbox`), and a firebrick `Include` in
`~/.ssh/config`. A smoke test must never touch any of them.

`smoke-env.sh` next to this file creates the environment and cleans it up. It
puts everything under a short `/tmp/firebrick-smoke.XXXX` directory, because
unix socket paths are limited to 108 bytes and the scratchpad path is too long:

| Variable          | Isolates                                                     |
| ----------------- | ------------------------------------------------------------ |
| `XDG_RUNTIME_DIR` | the daemon socket, so the CLI starts its own `fbkd`          |
| `MSB_HOME`        | microsandbox's runtime, database, images and sandboxes       |
| `XDG_DATA_HOME`   | SSH keys, the generated SSH config and `secrets.yml`         |
| `XDG_STATE_HOME`  | the daemon log, and the microsandbox home without `MSB_HOME` |
| `XDG_CONFIG_HOME` | the VS Code-family and Zed settings fbkd syncs on Linux      |
| `HOME`            | the `Include` line fbkd adds to `~/.ssh/config`              |

## Steps

### 1. Build

```bash
cargo build
```

The CLI starts the `fbkd` that sits next to its own binary, so
`target/debug/fbk` always runs the daemon you just built.

### 2. Create the environment

```bash
.claude/skills/smoke-test/smoke-env.sh up
```

It prints the path of an env file, such as `/tmp/firebrick-smoke.Ab12/env`.
Shell state doesn't carry over between commands, so start **every** command that
runs `fbk`, `msb` or `ssh` with `source <env file>`. The env file sets:

- `$FIREBRICK` - the CLI from `target/debug`.
- `$MSB` - microsandbox's `msb` in the isolated home. It exists after the first
  `fbk` command, because fbkd installs the runtime when it starts.
- `$SMOKE_WORK` - the directory for workspaces.
- `$SMOKE_SSH_CONFIG` - the generated SSH config.

### 3. Create a workspace

Make a directory per scenario under `$SMOKE_WORK`. Its name is the leaf of the
workspace path, so `$SMOKE_WORK/demo` gets the host name `demo.fbk`. Add a
`.firebrick.yml` when the scenario needs a name, image or setting:

```bash
source /tmp/firebrick-smoke.Ab12/env
mkdir -p "$SMOKE_WORK/demo" && cd "$SMOKE_WORK/demo"
printf 'name: smoke-demo\nimage: ubuntu:26.04\ninit: false\n' > .firebrick.yml
```

Pick the image deliberately:

- **No `image`**: the default `ghcr.io/wmeints/firebrick-base:v<version>`. This
  only works when that version is published, so not after a version bump.
- **`ubuntu:26.04`**: boots fast, but needs `init: false` because it has no
  `/sbin/init`. It also has no `agent` user, so SSH fails with `guest user not
  found: agent`.
- **A locally built image**: see "Test a local image" below.

A fresh `MSB_HOME` has no image cache, so the first `fbk start` pulls the image.
That takes up to a minute.

### 4. Run the scenario

Run the smallest set of commands that shows the behavior. Use `timeout` so a
hang doesn't block the session, and give `fbk run` a non-terminal stdin:

```bash
source /tmp/firebrick-smoke.Ab12/env && cd "$SMOKE_WORK/demo"
timeout 300 "$FIREBRICK" start; echo "exit=$?"
"$FIREBRICK" ls
timeout 60 "$FIREBRICK" run -- sh -c 'id; pwd' </dev/null; echo "exit=$?"
timeout 60 ssh -F "$SMOKE_SSH_CONFIG" -o BatchMode=yes demo.fbk 'whoami'
```

Pass `-F "$SMOKE_SSH_CONFIG"` to `ssh`. OpenSSH reads `~/.ssh` from the password
database, not from `$HOME`, so it never sees the isolated config by itself.

When something fails, read the logs:

- the daemon: `$XDG_STATE_HOME/firebrick/fbkd.log.*`
- a sandbox: `$MSB_HOME/sandboxes/<name>/logs/`
- microsandbox directly: `"$MSB" ls`, `"$MSB" logs <name>`

**After changing the daemon code**, rebuild and stop this environment's daemon,
so the next `fbk` command starts the new build. Sandboxes keep running while the
daemon restarts.

```bash
cargo build && source /tmp/firebrick-smoke.Ab12/env && kill $(lsof -t "$XDG_RUNTIME_DIR/fbkd.sock")
```

### 5. Clean up

Always clean up, also when the scenario failed:

```bash
.claude/skills/smoke-test/smoke-env.sh down /tmp/firebrick-smoke.Ab12/env
```

This stops and removes every sandbox in the environment, stops its daemon and
deletes the directory. It refuses any path that isn't a smoke-test environment.

### 6. Report

Tell the user which commands you ran, with their real output and exit codes, and
which image the sandbox used.

## Windows

On Windows, use `smoke-env.ps1` from PowerShell instead of `smoke-env.sh`. It
puts the environment in `%TEMP%\firebrick-smoke.XXXX`, and isolates what Windows
uses instead of `XDG_RUNTIME_DIR` and `HOME`:

| Variable                       | Isolates                                                     |
| ------------------------------ | ------------------------------------------------------------ |
| `FIREBRICK_PIPE_PREFIX`        | the daemon and vsock pipes, so the CLI starts its own `fbkd` |
| `USERPROFILE`                  | the `Include` line fbkd adds to `~\.ssh\config`              |
| `FIREBRICK_EDITOR_CONFIG_ROOT` | the editor settings fbkd syncs in `%APPDATA%`                |

`MSB_HOME` and the `XDG_*` variables isolate the rest, as on Unix. Build with
`cargo build`, then:

```powershell
$envFile = .\.claude\skills\smoke-test\smoke-env.ps1 up
. $envFile
New-Item -ItemType Directory -Force "$env:SMOKE_WORK\demo"; Set-Location "$env:SMOKE_WORK\demo"
Set-Content .firebrick.yml "name: smoke-demo`nimage: ubuntu:26.04`ninit: false`n"
& $FIREBRICK start --yes
& $FIREBRICK ls
$null | & $FIREBRICK run -- sh -c 'id; pwd'
```

Dot-source the env file again in every new PowerShell command, as on Unix. A
command that runs `fbk` without it starts a daemon against the user's own
`%LOCALAPPDATA%\firebrick` and `~\.ssh\config`.

- Run `fbk` directly. `Start-Process -Wait` tracks the process tree in a job
  object without the breakaway right, so the `fbkd` that `fbk` starts can't
  start VMs, and the call waits for the daemon to exit.
- For `ssh`, which can hang, start it with `Start-Process` without `-Wait` and
  bound it with `WaitForExit`:

  ```powershell
  $ssh = Start-Process C:\Windows\System32\OpenSSH\ssh.exe -NoNewWindow -PassThru `
      -ArgumentList '-F', $env:SMOKE_SSH_CONFIG, '-o', 'BatchMode=yes', 'demo.fbk', 'whoami'
  if (-not $ssh.WaitForExit(60000)) { $ssh.Kill() }
  ```
- The daemon log is in `$env:XDG_STATE_HOME\firebrick\fbkd.log.*`.
- Windows locks a running `fbkd.exe`, so stop this environment's daemon before
  `cargo build` replaces it: `smoke-env.ps1 down` does, or rename
  `target\debug\fbkd.exe` aside, which Windows allows for a running program.

Clean up with:

```powershell
.\.claude\skills\smoke-test\smoke-env.ps1 down $envFile
```

It removes the sandboxes and stops only the `fbkd` that serves the environment's
pipe, then deletes the directory. It refuses any path that isn't a smoke-test
environment.

## Test a local image

To test changes to the `Dockerfile` before the image is published, build it with
Docker and load it into the isolated microsandbox home. No registry is needed.
Run this from the repository root:

```bash
source /tmp/firebrick-smoke.Ab12/env
docker build -q -t firebrick-base:smoke .
docker save firebrick-base:smoke -o "$SMOKE_ROOT/image.tar"
"$MSB" load -q -i "$SMOKE_ROOT/image.tar" -t firebrick-base:smoke
```

`$MSB` only exists after fbkd has started once, so run `"$FIREBRICK" ls` first
in a new environment. Then use `image: firebrick-base:smoke` in the workspace's
`.firebrick.yml`. After cleaning up, remove the Docker tag with `docker rmi
firebrick-base:smoke`.

## Guardrails

- **Never touch the user's environment.** Don't stop or kill an `fbkd` you
  didn't start, don't run `fbk` or `msb` without sourcing the env file, and
  don't change `~/.local/state/firebrick` (the user's sandboxes and logs),
  `~/.microsandbox`, `~/.local/share/firebrick` or `~/.ssh/config`.
- **Never kill daemons by name.** `pkill fbkd` also stops the user's daemon.
  Stop only the process that listens on the environment's socket.
- **Don't leave sandboxes behind.** Run step 5 before you finish the turn.
- **Report real output.** If a step couldn't run, say so instead of assuming it
  would have passed.
