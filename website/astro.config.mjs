// @ts-check
import starlight from "@astrojs/starlight";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "astro/config";
import starlightLinksValidator from "starlight-links-validator";

// GitHub Pages serves the site at the root of its custom domain.
export default defineConfig({
  site: "https://firebrick-sandbox.dev",
  integrations: [
    starlight({
      title: "Firebrick",
      customCss: ["./src/styles/global.css"],
      // Overrides the parts of Starlight the design changes; global.css
      // themes the rest.
      components: {
        Header: "./src/components/starlight/Header.astro",
        PageTitle: "./src/components/starlight/PageTitle.astro",
        Sidebar: "./src/components/starlight/Sidebar.astro",
        TableOfContents: "./src/components/starlight/TableOfContents.astro",
        ThemeProvider: "./src/components/starlight/ThemeProvider.astro",
      },
      editLink: {
        baseUrl: "https://github.com/wmeints/firebrick/edit/main/website/",
      },
      // Dark code blocks in both themes, with wrapping instead of scrollbars.
      expressiveCode: {
        themes: ["github-dark"],
        useStarlightDarkModeSwitch: false,
        useStarlightUiThemeColors: false,
        defaultProps: { wrap: true },
        styleOverrides: {
          borderRadius: "var(--radius-card)",
          borderColor: "var(--color-card-border)",
          codeBackground: "var(--color-card)",
          codeFontFamily: "var(--font-mono)",
          uiFontFamily: "var(--font-mono)",
          frames: {
            editorTabBarBackground: "var(--color-card)",
            editorActiveTabBackground: "var(--color-card)",
            terminalTitlebarBackground: "var(--color-card)",
            terminalBackground: "var(--color-card)",
            terminalTitlebarBorderBottomColor: "var(--color-card-border)",
            terminalTitlebarDotsOpacity: "0",
            frameBoxShadowCssValue: "none",
          },
        },
      },
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/wmeints/firebrick",
        },
      ],
      // Lists the docs in reading order. A slug without a page fails the build.
      sidebar: [
        "docs",
        "docs/installation",
        "docs/quickstart",
        "docs/configuration",
        "docs/networking",
        "docs/secrets",
        "docs/editor-support",
        "docs/custom-images",
        "docs/command-line-reference",
      ],
      // Fails the build on broken internal links.
      plugins: [starlightLinksValidator()],
    }),
  ],
  vite: {
    plugins: [tailwindcss()],
  },
});
