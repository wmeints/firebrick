# 34. Serve the website from firebrick-sandbox.dev

## Status

Accepted

## Context

GitHub Pages serves the website at `https://wmeints.github.io/firebrick/`
([ADR 0032](0032-publish-the-website-on-github-pages.md)), so `website/` builds
it under the `/firebrick` base path. That URL depends on the repository's owner
and name, and every hand-written link in the docs carries the base path.
Firebrick now has its own domain, `firebrick-sandbox.dev`.

## Decision

GitHub Pages serves the site at the root of `https://firebrick-sandbox.dev`.

- `site` in `website/astro.config.mjs` is `https://firebrick-sandbox.dev`, and
  there's no `base`.
- Links in the Markdown docs are root-relative, such as `/docs/secrets/`.
- `.astro` files keep building links from `import.meta.env.BASE_URL`, which is
  now `/`.
- The end-to-end tests serve the build at the root, as GitHub Pages does.
- The maintainer sets the custom domain in the repository's Pages settings and
  points the domain's DNS at GitHub Pages. The `actions/deploy-pages` workflow
  doesn't need a `CNAME` file.

## Consequences

- The site's URL no longer changes when the repository is renamed or moved.
- `https://wmeints.github.io/firebrick/` redirects to the custom domain once
  it's set in the Pages settings.
- Moving the site under a path again needs a `base` and new links in the
  Markdown docs; the `.astro` files already follow `BASE_URL`.
