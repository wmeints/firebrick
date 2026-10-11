# Firebrick

Firebrick (`fbk`) is an agentic sandbox: it runs coding agents safely inside a
microVM on your own machine. Each project gets its own lightweight VM, built
from an OCI image, with only the project directory shared from the host. You
don't need a cloud account, a commercial license or root permissions.

Firebrick supports two ways of working:

- **Terminal agents** such as [Claude Code](https://claude.ai/code),
  [OpenCode](https://opencode.ai) and [Oh-my-pi](https://omp.sh): run them in
  the sandbox with `fbk run`.
- **IDE-integrated agents** such as GitHub Copilot: connect your IDE to the
  sandbox over SSH.

> [!NOTE]
> Firebrick is early in development.

## Installing and using Firebrick

The user documentation lives at
**[firebrick-sandbox.dev](https://firebrick-sandbox.dev/docs/)**. Start with
[Installation](https://firebrick-sandbox.dev/docs/installation/) to install the
`fbk` and `fbkd` binaries, then follow the
[Quickstart](https://firebrick-sandbox.dev/docs/quickstart/) to run an agent in
a sandbox.

The rest of this README is for people who work on the source code.

## Development

### Setup

Install the toolchain (Rust, Node, pnpm, `buf`, `protoc`, `dprint`,
`actionlint`, `lefthook`, the GitHub CLI and Claude Code) with
[mise](https://mise.jdx.dev). This also installs the git hooks:

```sh
mise install
```

### Commands

| Command                   | Description                                   |
| ------------------------- | --------------------------------------------- |
| `cargo build`             | Build the `fbk` and `fbkd` binaries.          |
| `cargo unit-tests`        | Run the unit tests.                           |
| `cargo integration-tests` | Run the integration tests that boot real VMs. |
| `cargo lint`              | Run the linter.                               |
| `cargo fmt --all`         | Format the code.                              |

Cargo commands in this repository use `/tmp/firebrick-msb` as the microsandbox
home (`MSB_HOME`, set in `.cargo/config.toml`), so the integration tests don't
share a runtime or database with an installed `fbkd` or `msb`. Several test
runs, for example from two worktrees, can use it at the same time: each run
names its sandboxes after its process id, and removes the sandboxes left behind
by runs that were killed.

### Running a development build

The default sandbox image is the `firebrick-base` image of the same release, so
it doesn't exist for a version that hasn't been released yet. To run a
development build, push an image built from the [`Dockerfile`](Dockerfile) to a
local registry and set `image` in `.firebrick.yml` to it:

```sh
docker run -d -p 127.0.0.1:5000:5000 --name registry registry:2
docker build -t localhost:5000/firebrick-base:dev .
docker push localhost:5000/firebrick-base:dev
```

The local registry speaks plain HTTP, so allow it in `config.json` in the
microsandbox home (`$MSB_HOME`, or `~/.local/state/firebrick/msb`) before `fbkd`
starts:

```json
{ "registries": { "hosts": { "localhost:5000": { "insecure": true } } } }
```

### Workspace layout

The workspace contains five crates:

| Crate              | Folder          | Purpose                                                                                     |
| ------------------ | --------------- | ------------------------------------------------------------------------------------------- |
| `firebrick-cli`    | `crates/cli`    | The `fbk` CLI.                                                                              |
| `firebrick-daemon` | `crates/daemon` | The `fbkd` daemon.                                                                          |
| `firebrick-proto`  | `crates/proto`  | Generated gRPC code for the daemon API.                                                     |
| `firebrick-spec`   | `crates/spec`   | Parses and validates `.firebrick.yml`.                                                      |
| `firebrick-utils`  | `crates/utils`  | Shared paths for the socket, logs, SSH files and microsandbox home, and the name sanitizer. |

The gRPC contract lives in
[`crates/proto/proto/daemon.v1.proto`](crates/proto/proto/daemon.v1.proto).

### Website

The website lives in [`website/`](website/), an Astro site with Starlight for
the docs and Tailwind for styling. Run its commands with pnpm from the
repository root:

| Command                                        | Description                                      |
| ---------------------------------------------- | ------------------------------------------------ |
| `pnpm --dir website install --frozen-lockfile` | Install the dependencies.                        |
| `pnpm --dir website run dev`                   | Preview the site on `http://localhost:4321/`.    |
| `pnpm --dir website run build`                 | Build the static site into `website/dist`.       |
| `pnpm --dir website run lint`                  | Run ESLint.                                      |
| `pnpm --dir website run check`                 | Type-check the site.                             |
| `pnpm --dir website run format`                | Format the code with Prettier.                   |
| `pnpm --dir website run test`                  | Run the unit tests.                              |
| `pnpm --dir website run test:e2e`              | Run the end-to-end tests against the built site. |

## Documentation

- [User documentation](https://firebrick-sandbox.dev/docs/) - installing,
  configuring and using Firebrick. Its source is in
  [`website/src/content/docs/docs/`](website/src/content/docs/docs/).
- [Architecture](docs/architecture/01-introduction-and-goals.md) - the arc42
  architecture documentation.
- [Decisions](docs/architecture/decisions/) - architecture decision records.
- [CLAUDE.md](CLAUDE.md) - coding guidelines and the definition of done.

## License

Firebrick is licensed under the [MIT License](LICENSE).
