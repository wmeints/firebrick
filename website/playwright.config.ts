import { defineConfig, devices } from "@playwright/test";

const port = 4321;

// Runs the end-to-end tests against the built site, served by `astro preview`
// at the root, as on GitHub Pages. Run `pnpm run build` first.
export default defineConfig({
  testDir: "tests/e2e",
  forbidOnly: true,
  reporter: process.env.CI ? "github" : "list",
  use: { baseURL: `http://localhost:${port}/` },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    // Under a coding agent `astro preview` detaches into a background server
    // unless `--ignore-lock` is set, and Playwright then sees it exit.
    command: `pnpm exec astro preview --port ${port} --ignore-lock`,
    url: `http://localhost:${port}/`,
    reuseExistingServer: false,
  },
});
