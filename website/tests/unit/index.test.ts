import { experimental_AstroContainer as AstroContainer } from "astro/container";
import { beforeAll, expect, test } from "vitest";
import Index from "../../src/pages/index.astro";

// `vitest.config.ts` passes the base path on from `astro.config.mjs`, so these
// tests check how the page builds its links, not what the Astro build sets
// `BASE_URL` to. The end-to-end tests check the links of the built site.
let html = "";

beforeAll(async () => {
  const container = await AstroContainer.create();
  html = await container.renderToString(Index);
});

test("the home page links to the docs under the base path", () => {
  expect(html).toContain('href="/docs/"');
  expect(html).toContain('href="/docs/quickstart/"');
});

test("the home page links to the repository and the releases", () => {
  expect(html).toContain('href="https://github.com/wmeints/firebrick"');
  expect(html).toContain(
    'href="https://github.com/wmeints/firebrick/releases"',
  );
});

test("the documentation cards show the pages from the docs", () => {
  for (const page of ["configuration", "secrets", "command-line-reference"]) {
    expect(html).toContain(`href="/docs/${page}/"`);
  }
  expect(html).toContain(
    "Go from the installed binaries to an agent running in a sandbox.",
  );
});

test("the sections appear in the order of the design", () => {
  const headings = [
    "A fireproof room for your coding agent.",
    "Containers share a kernel. Your agent shouldn&#39;t.",
    "From project folder to sealed sandbox in one command.",
    "Read the Firebrick Book.",
    "Ready to put your agent in a sandbox?",
    "Agentic sandboxing for coding agents.",
  ];

  const positions = headings.map((heading) => html.indexOf(heading));

  expect(positions).not.toContain(-1);
  expect(positions).toEqual([...positions].sort((a, b) => a - b));
});

test("the terminal shows only real fbk commands", () => {
  const text = html.replace(/<[^>]+>/g, "");
  const commands = [...text.matchAll(/\$ (fbk [a-z]+)/g)].map(
    (match) => match[1],
  );

  expect(commands.length).toBeGreaterThan(0);
  for (const command of commands) {
    expect(["fbk start", "fbk run", "fbk stop", "fbk rm", "fbk ls"]).toContain(
      command,
    );
  }
});
