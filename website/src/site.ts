/** The GitHub repository of Firebrick. */
export const repository = "https://github.com/wmeints/firebrick";

/** The GitHub links the landing page and the docs share. */
export const links = {
  repository,
  releases: `${repository}/releases`,
  newIssue: `${repository}/issues/new`,
  license: `${repository}/blob/main/LICENSE`,
};

/** Prefixes a path on the site with Astro's `base`, so links survive a move. */
export function withBase(path: string): string {
  return `${import.meta.env.BASE_URL}/${path}`.replace(/\/+/g, "/");
}

/** A page of the docs, as the landing page links to it. */
export interface DocsPage {
  title: string;
  description: string;
  href: string;
}

// The frontmatter of the docs pages, by their path under `content/docs`.
// Read through Vite rather than `astro:content`, which is empty in the unit tests.
const pages = import.meta.glob<{ title: string; description?: string }>(
  "./content/docs/**/*.md",
  { eager: true, import: "frontmatter" },
);

/**
 * Looks up the docs page with the content collection `id`, such as
 * `docs/quickstart`. Throws when the page doesn't exist, so a broken link on
 * the landing page fails the build.
 */
export function docsPage(id: string): DocsPage {
  const frontmatter =
    pages[`./content/docs/${id}.md`] ?? pages[`./content/docs/${id}/index.md`];
  if (!frontmatter) throw new Error(`The docs have no page "${id}".`);
  return {
    title: frontmatter.title,
    description: frontmatter.description ?? "",
    href: withBase(`${id}/`),
  };
}
