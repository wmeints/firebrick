# Module structure refactoring

Recommendations for restructuring the modules of the crates, so there are fewer
top-level modules and each one is a deep module behind a narrow interface. Line
counts include the tests unless noted otherwise. Nothing here has been
implemented yet.

## daemon: 21 → 8 top-level modules

**Done** (ADR 0057). The result differs from the plan below in a few places:

- `forward` got a fourth file, `listener.rs`, with the loopback listeners that
  `Forwards` and `CallbackForwards` share.
- `host_config::sync` takes a list of `host_config::Host` values, so
  `host_config` doesn't depend on microsandbox; `sandboxes::sync_ssh_config`
  builds them.
- The clipboard helpers that both `protocol` and `host_tools` need
  (`read_at_most`, `kinds_where`) stay in `clipboard/mod.rs`, and `read_events`
  moved to `netlog/parse.rs`.
- The tests of `SandboxManager`'s public methods stay in `sandboxes/mod.rs`;
  only the tests of private functions moved with them.

`sandboxes.rs` (3425 lines) coordinates everything: it imports 14 of the other
modules. Most of those modules have only one or two users, so they can become
private submodules behind `SandboxManager`.

```
lib.rs
├── runtime.rs                  unchanged
├── ssh.rs                      keys + host names only (config writing moves to host_config)
├── secrets.rs                  unchanged (main.rs and tests/grpc.rs use SecretStore)
├── server/                     was server.rs + session.rs + tunnel.rs
│   ├── mod.rs                  FirebrickServer, the service impl, run/serve
│   ├── convert.rs              proto ↔ domain conversions (server.rs:155–445, 839–893)
│   ├── transport.rs            accept_loop, Connection (server.rs:959–1042)
│   ├── shutdown.rs             the shutdown_signal/ctrl_c/console_close functions (cfg'd per platform)
│   ├── attach.rs               was session.rs
│   └── tunnel.rs
├── sandboxes/                  SandboxManager stays the only real entry point
│   ├── mod.rs                  SandboxManager + public request/response types
│   ├── error.rs                SandboxError + create_error/registry_error/...
│   ├── status.rs               current_status, pause-state probing, list_all + retry
│   ├── recreate.rs             Change, Recreation, snapshots, upgrade probes
│   ├── settings.rs             SandboxSettings, labels, mounts, resources, init
│   ├── ports.rs                PortsChange, stored/save_ports
│   ├── secret_updates.rs       set/remove_secret, SecretChange, scope updates
│   └── provision/              env, git, mise, pull, network (was 5 top-level modules)
├── guest/                      channels the guest uses to make the host act
│   ├── mod.rs                  re-exports Clipboards, NetLogs, Relays, HostClipboard, Opener
│   ├── vsock.rs, rate_limit.rs
│   ├── clipboard/{mod, protocol, host_tools}.rs
│   ├── netlog/{mod, parse, record}.rs
│   └── open.rs
├── forward/
│   ├── mod.rs                  labels, Connector, Forwards
│   ├── callback.rs             CallbackForwards + Activity
│   └── ssh.rs                  SshConnector/SshClient
└── host_config/                one function: sync(hostnames, sandboxes)
    ├── ssh_config.rs           ssh::sync_config moves here
    ├── vscode.rs, zed.rs
    └── settings_file.rs, atomic_file.rs
```

Why:

- **`host_config`:** `sandboxes::sync_ssh_config` already updates the SSH
  config, VS Code and Zed together (`sandboxes.rs:2450–2510`). One
  `host_config::sync` would replace five modules, and the support code
  (`settings_file`, `atomic_file`) becomes private. It also takes the editor
  code out of `sandboxes.rs`.
- **`guest`:** `clipboard`, `netlog` and `open` all follow the same pattern: a
  vsock or exec channel, a per-sandbox listener and a rate limit. `vsock` and
  `rate_limit` are their shared plumbing and can be private to `guest`.
  - **`clipboard.rs` split:** it is 1139 lines before the tests and mixes two
    concerns. The guest wire protocol (Request/Reply/Channel/serve, lines
    205–590) goes in `protocol.rs`. Driving `wl-copy`, `xclip` and `osascript`
    (HostTools/Desktop, lines 590–1139) goes in `host_tools.rs`.
  - **`netlog.rs` split:** parsing lines (lines 103–180) and the
    Aggregate/Record model and its persistence (lines 181–515) separate cleanly
    from the listener (`NetLogs`).
- **`provision`:** `env`, `git`, `mise`, `pull` and `network` each apply one
  part of the spec to a sandbox. The server only needs a few of their types
  (`MiseUpdate`, `PullUpdate`, `GitIdentity`, `network::allows_host`).
  `sandboxes` can re-export those, and the modules themselves become private.
  `network` is also used by `open`, so it needs to be `pub(crate)`.
- **`server`:** `session` and `tunnel` are gRPC stream adapters that only the
  server uses. Inside `server.rs`, the conversion functions are about 350 lines
  of glue that hide the service impl.
- **Narrower crate interface:** `lib.rs` exports 17 `pub mod`s, but `main.rs`
  and `tests/` only use `api`, `runtime`, `sandboxes`, `server`, `ssh`,
  `secrets`, `clipboard`, `open` and `forward::PORTS_LABEL`. Make everything
  else private, and update `tests/grpc.rs` to the new paths or keep a small
  re-export.

## cli: 17 → 10 top-level modules

**Done** (ADR 0057). The result differs from the plan below in a few places:

- `resolve_spec` asks the daemon whether a sandbox with the legacy name exists,
  so it lives in `sandbox/workspace.rs`; `spec_file` only holds the file
  handling and the name derivation it uses.
- `ensure_running` and the progress stream helpers (`follow_pull`, `Streamed`)
  live in `sandbox/start.rs`, which `upgrade` uses.
- The `approval` tests go through `Approvals` and `Approver` with a shared
  fixture, so they stay in `approval/mod.rs`; only the code moved.

```
lib.rs
├── client.rs
├── sandbox/                    was manage.rs (2384 lines) + git.rs
│   ├── mod.rs                  pub start/stop/remove/upgrade/list
│   ├── start.rs                start flow, build_start_request, approve_new_sandbox
│   ├── host_paths.rs           resolve_mounts, expand_tilde, user_home, dotfiles_dir, mise_config
│   ├── upgrade.rs              upgrade + follow_pull/Streamed
│   ├── list.rs                 OutputFormat, table/json rendering
│   ├── workspace.rs            pub(crate): ensure_running, check_workspace_owner, describe_status
│   └── git.rs
├── spec_file/                  everything about .firebrick.yml on disk
│   ├── mod.rs                  SPEC_FILE_NAME, load/migrate/spec_error, resolve_spec + name derivation
│   ├── init.rs
│   └── validate.rs
├── network/                    update.rs, log.rs, presets.rs
├── port.rs
├── secret.rs
├── approval/                   store.rs (Approvals), diff.rs (difference/listing), prompt.rs
├── remote/                     session.rs (+ vt.rs), ssh.rs — streams into the sandbox
├── ui/                         table.rs, progress.rs
└── #[cfg(test)] test_support/  fake_daemon.rs, symlinked_spec.rs
```

Why:

- **`spec_file`:** handling the spec file is spread across `manage`
  (`SPEC_FILE_NAME`, `resolve_spec`, `default_name`/`hashed_name`/
  `legacy_name`), `validate` (`load_spec`, `migrate_spec`) and `init`. `port`,
  `network` and `secret` reach into `manage` for those helpers, which is why
  `manage` has seven `pub(crate)` functions. Moving them into `spec_file` lets
  `sandbox` be about the daemon's sandbox lifecycle only.
- **`network`:** `presets` is only used by the network commands. `network.rs`
  holds two separate commands, `update` (lines 28–286) and `log` (lines
  287–388).
- **`approval`:** it is 504 lines before the tests, with four parts: the
  approvals file store, the diff rendering, the `Approver` and the `Prompt`.
  This is lower priority than the others.
- **`test_support`:** `fake_daemon` and `symlinked_spec` are both test fixtures;
  keep them together under `#[cfg(test)]`. `tests/host_git.rs` uses
  `firebrick_cli::git`, so it needs `sandbox::git` to be public, or a re-export.

## spec: split the 3115-line `lib.rs`

Everything is in `lib.rs`. Split it into private modules and re-export them flat
from `lib.rs`, so callers don't change:

| Module          | Contents                                                                   |
| --------------- | -------------------------------------------------------------------------- |
| `error.rs`      | SandboxSpecError, SpecDiagnostic, SizeError                                |
| `network.rs`    | NetworkSpec, NetworkRule + parsing (258–428)                               |
| `ports.rs`      | PortMapping + its serde (429–554)                                          |
| `ports_edit.rs` | `with_port`/`add_port`/`remove_port`, PortsBlock, Lines (1197–1416)        |
| `env.rs`        | env deserialization and name validation (555–710)                          |
| `mounts.rs`     | MountSpec, guest paths, duplicates (766–878)                               |
| `names.rs`      | sandbox name and image validation, size parsing                            |
| `file.rs`       | symlink-safe `from_file`/`read_text`/`write_text`, platform `open_checked` |
| `migrate.rs`    | `migrate_mise`, `boolean_value`                                            |

`ports_edit.rs` is a line-based YAML editor, which is why it is separate from
`ports.rs`. The test module at `lib.rs:1445` is `pub mod tests`; it only needs
to be `mod tests`.

## utils: split `lib.rs` by concern

`lib.rs` is 635 lines before the tests and covers four concerns:

- **`paths.rs`:** BaseDir, the `*_dir` and `*_path` functions, the socket
  directories and the `NO_*_DIR` messages.
- **`private_fs.rs`:** `write_private`, `make_private_dir`, `create_private_dir`
  and `restrict_dir`. `create_private_dir` (unix only) and `make_private_dir`
  have confusingly similar names, so check whether one can absorb the other.
- **`process.rs`:** `try_lock_daemon` and `keep_stdio_from_children`.
- **`text.rs`:** `sanitize_label` and `strip_control`.

`is_trusted_peer` is only used by `ipc/unix.rs`, so it belongs in `ipc`. The
daemon's `atomic_file` could probably move into `private_fs` as well.

## Approach

1. **Order:** the biggest gains are `host_config` and `guest` in the daemon,
   `spec_file` plus the `manage` split in the CLI, and `spec`'s `lib.rs`.
   `approval`, `ui` and `remote` are optional cleanup.
2. **Moves before splits:** do pure moves (`git mv` plus path fixes) in separate
   commits from splitting files, so the review diffs stay readable.
3. **Docs:** `docs/architecture/06-runtime-view.md` and
   `docs/architecture/concepts/threat-model.md` name module files such as
   `sandboxes.rs` and `netlog.rs`, so they need updating. The definition of done
   probably also calls for an ADR for the new layout.
4. **Tests:** the restructure touches `crates/daemon`, so the definition of done
   requires the `vm-tests` to pass.
