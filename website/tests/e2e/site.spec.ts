import { expect, test, type Page } from "@playwright/test";

const base = "/firebrick/";

/** What the crawl found on one page or asset. */
interface Visit {
  path: string;
  status: number | undefined;
  /** Pixels the page scrolls horizontally; undefined for non-HTML responses. */
  overflow?: number;
}

// Visits every page reachable from the home page on a 375px wide screen, and
// requests every asset those pages reference. Crawls once for all tests.
async function crawl(page: Page): Promise<Visit[]> {
  await page.setViewportSize({ width: 375, height: 812 });
  const visits = new Map<string, Visit>();
  const queue = [base];
  while (queue.length > 0) {
    const path = queue.shift() as string;
    if (visits.has(path)) continue;
    const response = await page.goto(path);
    const visit: Visit = { path, status: response?.status() };
    visits.set(path, visit);
    if (!response?.headers()["content-type"]?.includes("text/html")) continue;
    visit.overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - window.innerWidth,
    );
    queue.push(...(await internalUrls(page, "a[href]", "href")));
    for (const asset of await assetUrls(page)) {
      if (visits.has(asset)) continue;
      const assetResponse = await page.request.get(asset);
      visits.set(asset, { path: asset, status: assetResponse.status() });
    }
  }
  return [...visits.values()];
}

// Returns the paths of the stylesheets, icons, scripts and images on the page.
async function assetUrls(page: Page): Promise<string[]> {
  return [
    ...(await internalUrls(page, "link[href]", "href")),
    ...(await internalUrls(page, "[src]", "src")),
  ];
}

// Returns the paths in `attribute` of the elements matching `selector` that
// stay on the site.
async function internalUrls(
  page: Page,
  selector: string,
  attribute: "href" | "src",
): Promise<string[]> {
  const urls = await page
    .locator(selector)
    .evaluateAll(
      (elements, name) =>
        elements.map(
          (element) =>
            new URL(element.getAttribute(name) ?? "", document.baseURI).href,
        ),
      attribute,
    );
  const origin = new URL(page.url()).origin;
  return urls
    .map((url) => new URL(url))
    .filter((url) => url.origin === origin)
    .map((url) => url.pathname);
}

let visits: Visit[] = [];

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  visits = await crawl(page);
  await page.close();
});

test("every internal link and asset resolves under the base path", () => {
  const broken = visits.filter((visit) => visit.status !== 200);

  expect(broken).toEqual([]);
  expect(visits.map((visit) => visit.path)).toEqual(
    expect.arrayContaining([`${base}docs/`, `${base}favicon.svg`]),
  );
});

test("the docs sidebar lists the pages in reading order", async ({ page }) => {
  await page.goto(`${base}docs/`);

  const titles = await page
    .locator("#starlight__sidebar ul.top-level a")
    .evaluateAll((links) => links.map((link) => link.textContent?.trim()));

  expect(titles).toEqual([
    "Introduction",
    "Installation",
    "Quickstart",
    "Configuration",
    "Networking",
    "Secrets",
    "Editor support",
    "Custom images",
    "Command-line reference",
  ]);
});

test("no page scrolls horizontally on a 375px wide screen", () => {
  const overflowing = visits.filter((visit) => (visit.overflow ?? 0) > 0);

  expect(overflowing).toEqual([]);
});
