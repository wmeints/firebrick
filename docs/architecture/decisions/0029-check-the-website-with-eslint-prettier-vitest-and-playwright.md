# 29. Check the website with ESLint, Prettier, Vitest and Playwright

## Status

Accepted. The Claude Code stop hook this record mentions has been removed; the
website's end-to-end tests now run by hand and in CI.

## Context

The Rust code has a formatter, a linter that fails on warnings, unit tests and
integration tests, enforced by a Claude Code stop hook, lefthook and CI. The
website (ADR 0027) needs the same guard rails before agents build the docs and
the landing page on it. The risks specific to the site are links that ignore the
GitHub Pages base path, pages that scroll sideways on a phone, and accessibility
mistakes in hand-written markup.

## Decision

`website/package.json` defines one script per check:

- `format:check`: Prettier, with the Astro and Tailwind plugins, which also
  sorts Tailwind classes. dprint keeps formatting the Markdown (ADR 0008), so
  Prettier ignores `*.md`.
- `lint`: ESLint with the recommended and strict `typescript-eslint` rules and
  `eslint-plugin-astro`'s recommended and strict accessibility rules, failing on
  any warning.
- `check`: `astro check`, which type-checks `.astro` and TypeScript files,
  failing on any warning.
- `test`: Vitest unit tests in `tests/unit/`, which render components with
  Astro's container API under the configured base path.
- `build`: the Astro build, which fails on broken docs links.
- `test:e2e`: Playwright end-to-end tests in `tests/e2e/` against the built
  site, served by `astro preview` under the base path. They crawl every page
  reachable from `/` once, fail on a link or asset (stylesheet, icon, script,
  image) that doesn't resolve, and fail when a page scrolls horizontally at
  375px. `test.only` is rejected, so a focused test can't skip the others.

The stop hook runs all of them when `website/` or `mise.toml` has uncommitted
changes, lefthook runs all but the end-to-end tests before a commit that touches
`website/` or `mise.toml`, and `.github/workflows/website.yaml` runs all of them
on pull requests and pushes to `main`. A `website-reviewer` agent joins the
`review-branch` workflow for what the tools can't check.

## Consequences

- A broken base path or a horizontal scrollbar fails before review, on every
  page the crawler reaches.
- The end-to-end tests need a Chromium that Playwright downloads once (`pnpm
  --dir website exec playwright install chromium`); the stop hook and CI install
  it when it's missing.
- Astro 7 detaches `astro preview` into a background server when it runs under a
  coding agent, so the Playwright config starts it with `--ignore-lock`.
- Vitest sets `import.meta.env.BASE_URL` to `/` itself; `vitest.config.ts`
  passes the base path from `astro.config.mjs` on.
- The website checks add about 15 seconds to a stop with website changes.
