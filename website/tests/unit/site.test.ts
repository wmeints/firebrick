import { expect, test } from "vitest";
import { docsPage } from "../../src/site";

test("docsPage links to a docs page under the base path", () => {
  const page = docsPage("docs/quickstart");

  expect(page).toEqual({
    title: "Quickstart",
    description:
      "Go from the installed binaries to an agent running in a sandbox.",
    href: "/docs/quickstart/",
  });
});

test("docsPage links the introduction to the docs root", () => {
  expect(docsPage("docs").href).toBe("/docs/");
});

test("docsPage fails the build for a page that doesn't exist", () => {
  expect(() => docsPage("docs/missing")).toThrow(
    'The docs have no page "docs/missing".',
  );
});
