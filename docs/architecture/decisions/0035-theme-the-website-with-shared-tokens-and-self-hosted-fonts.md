# 35. Theme the website with shared tokens and self-hosted fonts

## Status

Accepted

## Context

The landing page and the docs follow one visual design (#77, mockups in
`designs/`): a dark landing page, docs with a dark header and a light content
area, Bricolage Grotesque headings, IBM Plex Sans body text and IBM Plex Mono
code. The site may make no third-party requests, so the fonts can't come from
Google Fonts. Starlight must stay upgradable, so its source can't be copied. The
landing page links to docs pages, and a broken link must fail the build.

## Decision

- The design tokens are defined once, in a Tailwind `@theme static` block in
  `website/src/styles/global.css`, Starlight's `customCss`. `static` emits every
  token as a CSS custom property, so the Starlight variables (`--sl-color-*`)
  for the light and dark themes are mapped to the same tokens the landing page
  uses as Tailwind utilities (`bg-ink`, `font-display`, ...).
- The fonts come from the `@fontsource/bricolage-grotesque`,
  `@fontsource/ibm-plex-sans` and `@fontsource/ibm-plex-mono` packages, which
  bundle the font files, so they're served from the site itself. Only the Latin
  subsets and the weights the design uses are imported.
- Starlight is customized through `global.css` where CSS is enough, and through
  component overrides in `website/src/components/starlight/` otherwise: the
  `Header`, `Sidebar`, `PageTitle` and `TableOfContents` wrap or replace the
  defaults, and `ThemeProvider` makes the light theme the default for visitors
  who haven't chosen one, by storing `light` before Starlight reads the stored
  theme. Choosing "Auto" stores an empty value, which is kept.
- Expressive Code renders code blocks with one dark theme in both site themes,
  themed with the tokens, and wraps long lines instead of scrolling.
- The landing page uses Tailwind's preflight as its reset, imported only by
  `src/pages/index.astro` (`src/styles/landing.css`), because Starlight's reset
  covers only the docs.
- The landing page links to docs pages through `docsPage(id)` in
  `website/src/site.ts`. It reads the page's title and description from its
  frontmatter, so the cards stay in sync with the docs, and throws when the page
  doesn't exist, which fails the build. It reads the Markdown through
  `import.meta.glob` rather than `astro:content`, whose store is empty when
  Vitest renders the page.

## Consequences

- Changing a color or font in `global.css` changes the landing page and the docs
  together.
- The three font packages are new dependencies; they ship only font files and
  CSS, with no install scripts.
- A Starlight upgrade that changes the markup or class names of the overridden
  or restyled parts (search button, pagination, asides, sidebar) can break the
  look and needs a visual check.
- A visitor whose system prefers dark sees the light docs until they pick "Dark"
  or "Auto" in the theme menu.
