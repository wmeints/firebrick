# Constraints

## Technical constraints

- **Application must work on Mac/Linux/Windows:** We must ensure we cover enough
  operating systems to reach the target audience for the application.

- **Application runs without root permissions:** We must ensure that the
  application doesn't need any root permissions on the host to limit the impact
  of a breach in the sandbox.

## Conventions

- **Coding conventions:** we follow the coding conventions published as part of
  [clippy][LINTER] to ensure adequate formatting of the code. On top of the
  defaults, `clippy.toml` and `[workspace.lints]` in `Cargo.toml` limit
  functions to 4 arguments, 1 bool argument and 3 levels of nesting. The
  generated gRPC code in the `api` modules is exempt.

- **Website conventions:** the website in `website/` is formatted by Prettier
  and linted by ESLint and `astro check`, all failing on warnings. It is a
  static site under the GitHub Pages base path that makes no third-party
  requests. See
  [ADR 0029](decisions/0029-check-the-website-with-eslint-prettier-vitest-and-playwright.md).

- **Architecture documentation:** we use [Arc42][ARC42] style architecture
  documentation.

[LINTER]: https://doc.rust-lang.org/stable/clippy/usage.html
[ARC42]: https://docs.arc42.org/
