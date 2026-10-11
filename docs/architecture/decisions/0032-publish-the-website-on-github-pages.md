# 32. Publish the website on GitHub Pages

## Status

Accepted

## Context

The user documentation moved from `README.md` into the Starlight docs in
`website/`
([ADR 0027](0027-build-the-website-with-astro-starlight-and-tailwind.md)). The
site builds to static files and needs a host that users can link to. Firebrick
has no budget, domain or server of its own, and the site must make no
third-party requests.

`.github/workflows/website.yaml` already installs, checks, builds and tests the
site on every pull request and push to `main`
([ADR 0029](0029-check-the-website-with-eslint-prettier-vitest-and-playwright.md)).

## Decision

GitHub Pages serves the site at `https://wmeints.github.io/firebrick/`, which
`site` and `base` in `website/astro.config.mjs` already point at.

- The `check` job of `website.yaml` uploads `website/dist` with
  `actions/upload-pages-artifact` after the end-to-end tests pass, on pushes to
  `main` and on manual runs from `main`.
- A `deploy` job publishes that artifact with `actions/deploy-pages`. Only this
  job gets the `pages: write` and `id-token: write` permissions; the workflow
  keeps `contents: read`.
- Deployments share the `pages` concurrency group and aren't cancelled, so a
  deployment always finishes and the newest one wins.
- The repository's Pages source is set to "GitHub Actions" once by the
  maintainer; the workflow doesn't change repository settings.

## Consequences

- The published site always matches `main` and has passed the same checks as the
  pull request that changed it. Docs for features that haven't been released yet
  are published as soon as they merge.
- There's one version of the docs; versioned docs and a custom domain are left
  for later.
- The site's URL depends on the repository's owner and name. Renaming or moving
  the repository changes the URL and needs a new `site` and `base`.
- A push that doesn't touch `website/`, `mise.toml` or the workflow doesn't
  redeploy the site.
