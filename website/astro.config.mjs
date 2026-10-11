// @ts-check
import starlight from "@astrojs/starlight";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "astro/config";
import starlightLinksValidator from "starlight-links-validator";

// The site is served from the repository's GitHub Pages URL, so every link and
// asset path must include `base`.
export default defineConfig({
  site: "https://wmeints.github.io",
  base: "/firebrick",
  integrations: [
    starlight({
      title: "Firebrick",
      customCss: ["./src/styles/global.css"],
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
