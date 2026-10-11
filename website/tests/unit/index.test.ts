import { experimental_AstroContainer as AstroContainer } from "astro/container";
import { expect, test } from "vitest";
import Index from "../../src/pages/index.astro";

// `vitest.config.ts` passes the base path on from `astro.config.mjs`, so this
// test checks how the page builds its links, not what the Astro build sets
// `BASE_URL` to. The end-to-end tests check the links of the built site.
test("the home page links to the docs under the base path", async () => {
  const container = await AstroContainer.create();

  const html = await container.renderToString(Index);

  expect(html).toContain('href="/docs/"');
});
