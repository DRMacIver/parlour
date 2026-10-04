// The layout editors (DESIGN.md §3.5, §5), in a real browser: a member
// arranges their own front page at /settings/layout, and an admin arranges
// the default one at /admin/layout. Without JavaScript these are plain
// forms (add, remove, move up, move down); with it, the layout editor
// island drags spaces between sections and saves each change as it is
// made. Each project arranges a member of its own, and the admin's test
// only adds to the default front page what it takes away again, since the
// other tests run against the same forum at the same time.

import { expect, test, type Locator, type Page } from "@playwright/test";
import { expectFits, logIn, ownAddress, withJs } from "./helpers.js";

// Members whose front pages no other test looks at, one per project.
const ARRANGERS: Record<string, string> = {
  "no-js-320-light": "alma",
  "no-js-320-dark": "bram",
  "no-js-1280-light": "cleo",
  "no-js-1280-dark": "dara",
  "js-1280": "emil",
  "js-320": "fern",
};

// Logins are rate limited per account, so the admins' tests are shared out
// between two admins of their own.
const SECOND_HALF = new Set(["no-js-320-dark", "no-js-1280-dark", "js-320"]);

function arranger(): string {
  return ARRANGERS[test.info().project.name] ?? "alma";
}

function layoutAdmin(): string {
  return SECOND_HALF.has(test.info().project.name) ? "hal" : "gwen";
}

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

function section(page: Page, name: string): Locator {
  return page.locator(`section.layout-section[aria-label='${name}']`);
}

async function sectionNames(page: Page): Promise<string[]> {
  return page.locator("section.layout-section").evaluateAll((sections) =>
    sections.map((s) => s.getAttribute("aria-label") ?? ""),
  );
}

async function spacesIn(page: Page, name: string): Promise<string[]> {
  return section(page, name).locator(".layout-spaces li .admin-space-name a").allTextContents();
}

/** Waits for the island to take over from the plain forms, with JavaScript. */
async function editorReady(page: Page) {
  if (withJs()) {
    await expect(page.locator(".layout-editor[data-island-mounted]")).toBeAttached();
    await expect(page.locator(".layout-available")).toBeVisible();
  }
}

async function addSection(page: Page, name: string) {
  await page.fill("#new-section-name", name);
  await page.locator("button", { hasText: "Add section" }).click();
  if (!withJs()) {
    await expect(page.locator(".notice-success")).toHaveText("Section saved.");
  }
  await expect(section(page, name)).toBeVisible();
}

/**
 * Drags one element onto another, as the browser would: the drag's events
 * are sent to each in turn, so that it works wherever on the page the two
 * are, at any width.
 */
async function drag(page: Page, source: Locator, target: Locator) {
  const dataTransfer = await page.evaluateHandle(() => new DataTransfer());
  await source.dispatchEvent("dragstart", { dataTransfer });
  await target.dispatchEvent("dragenter", { dataTransfer });
  await target.dispatchEvent("dragover", { dataTransfer });
  await target.dispatchEvent("drop", { dataTransfer });
  await source.dispatchEvent("dragend", { dataTransfer });
}

/**
 * Puts a space in a section: with JavaScript by dragging it there, or with
 * the "Move to" choice beside it (`choose`); without, with the section's
 * "Add a space" choice. `label` is how the plain form's choice names it.
 */
async function putIn(page: Page, slug: string, into: string, label: string, choose = false) {
  const space = page.locator(`li[data-space='${slug}']`);
  if (withJs() && choose) {
    await space.locator("select").selectOption({ label: into });
  } else if (withJs()) {
    await drag(page, space, section(page, into).locator(".empty, .layout-spaces").first());
  } else {
    await section(page, into).locator("select[name=space]").selectOption({ label });
    await section(page, into).locator(".layout-add button").click();
    await expect(page.locator(".notice-success")).toHaveText("Saved.");
  }
  await expect(section(page, into).locator(`a[href='/c/${slug}']`)).toBeVisible();
}

test("a member arranges their own front page", async ({ page }) => {
  await logIn(page, arranger());
  await page.goto("/");
  await page.locator("a", { hasText: "Arrange your front page" }).click();
  await expect(page.locator("h1")).toHaveText("Your front page");
  await expect(page.locator(".toolbar-note")).toContainText("follows the default");
  await editorReady(page);
  await expectFits(page);

  // A new section, with a space from another section in it.
  await addSection(page, "Reading");
  await putIn(page, "books", "Reading", "Books & reading (in Hobbies)");
  await expect(section(page, "Hobbies").locator("a[href='/c/books']")).toHaveCount(0);
  await expectFits(page);

  // New sections go last; moving it up puts it above Feeds.
  let names = await sectionNames(page);
  expect(names.indexOf("Reading")).toBeGreaterThan(names.indexOf("Feeds"));
  await section(page, "Reading").locator(".panel-bar button", { hasText: "Move up" }).click();
  await expect
    .poll(async () => {
      const now = await sectionNames(page);
      return now.indexOf("Reading") < now.indexOf("Feeds");
    })
    .toBe(true);

  // Renaming it.
  await section(page, "Reading").locator("input[type=text]").fill("Books");
  await section(page, "Reading").locator("button", { hasText: "Rename" }).click();
  await expect(section(page, "Books")).toBeVisible();
  if (withJs()) {
    // Saved as it was made: a reload shows the same.
    await page.reload();
    await editorReady(page);
  }
  await expect(page.locator(".toolbar-note")).toContainText("arranged your front page yourself");

  // Moving a space within its section: Making things above Board games.
  if (withJs()) {
    await drag(
      page,
      section(page, "Hobbies").locator("li[data-space='making-things']"),
      section(page, "Hobbies").locator("li[data-space='board-games']"),
    );
  } else {
    const making = section(page, "Hobbies").locator("li", { has: page.locator("a[href='/c/making-things']") });
    await making.locator("button", { hasText: "Move up" }).click();
    await expect(page.locator(".notice-success")).toHaveText("Saved.");
  }
  await expect.poll(() => spacesIn(page, "Hobbies")).toEqual(["Making things", "Board games"]);
  await expectFits(page);

  // The front page shows what was arranged.
  await page.goto("/");
  const books = page.locator(".space-list", { has: page.locator("th a", { hasText: "Books" }) });
  await expect(books.locator("a[href='/c/books']")).toBeVisible();
  const hobbies = page.locator(".space-list", { has: page.locator("th a", { hasText: "Hobbies" }) });
  await expect(hobbies.locator("td .row-title a")).toHaveText(["Making things", "Board games"]);

  // Taking a space off the front page, and deleting a section.
  await page.goto("/settings/layout");
  await editorReady(page);
  const boardGames = section(page, "Hobbies").locator("li[data-space='board-games']");
  if (withJs() && test.info().project.name === "js-320") {
    // On a narrow screen, with the choice beside it.
    await boardGames.locator("select").selectOption({ label: "Off the front page" });
    await expect(page.locator(".layout-available a[href='/c/board-games']")).toBeVisible();
  } else if (withJs()) {
    await drag(page, boardGames, page.locator(".layout-available"));
    await expect(page.locator(".layout-available a[href='/c/board-games']")).toBeVisible();
  } else {
    const games = section(page, "Hobbies").locator("li", { has: page.locator("a[href='/c/board-games']") });
    await games.locator("button", { hasText: "Take off the front page" }).click();
    await expect(page.locator(".notice-success")).toHaveText("Saved.");
  }
  await expect(section(page, "Hobbies").locator("a[href='/c/board-games']")).toHaveCount(0);
  await section(page, "Books").locator("button, a", { hasText: "Delete section…" }).click();
  if (!withJs()) {
    await expect(page.locator("h1")).toHaveText("Delete the Books section?");
    await expect(page.locator(".member-list")).toContainText("Books & reading");
    await expectFits(page);
  }
  await page.locator("button", { hasText: "Delete the section" }).click();
  await expect(section(page, "Books")).toHaveCount(0);
  if (!withJs()) {
    await expect(page.locator(".notice-success")).toContainText("Section deleted.");
  }
  await page.goto("/");
  await expect(page.locator(".space-list a[href='/c/books']")).toHaveCount(0);
  await expect(page.locator(".space-list a[href='/c/board-games']")).toHaveCount(0);

  // Mutes are listed, and unmuted, here.
  await page.goto("/settings/layout");
  await expect(page.locator("#muted-heading + p")).toContainText("haven’t muted any");
  await page.goto("/c/introductions");
  await page.locator("form[action='/c/introductions/mute'] button").click();
  await expect(page.locator(".notice")).toContainText("muted this space");
  await page.goto("/settings/layout");
  const muted = page.locator("section[aria-labelledby='muted-heading']");
  await expect(muted.locator("a[href='/c/introductions']")).toBeVisible();
  await expectFits(page);
  await muted.locator("form[action='/c/introductions/unmute'] button").click();
  await expect(page.locator(".notice-success")).toHaveText("Unmuted.");
  await expect(muted.locator("a[href='/c/introductions']")).toHaveCount(0);
});

test("an admin arranges the default front page", async ({ page, browser }) => {
  const project = test.info().project.name;
  const slug = `layout-${project}`;
  const name = `Layout ${project}`;
  const extra = `Extra ${project}`;
  await logIn(page, layoutAdmin());

  // A space of this test's own, on no front page yet.
  await page.goto("/spaces/new");
  await page.fill("#name", name);
  await page.fill("#slug", slug);
  await page.locator("form[action='/spaces/new'] button[type=submit]").click();
  await expect(page.locator(".notice-success")).toContainText("The space is ready");
  await page.check("input[name=read_level][value=public]");
  await page.locator(`form[action='/c/${slug}/settings'] button[type=submit]`).click();
  await expect(page.locator(".notice-success")).toHaveText("Saved.");

  await page.goto("/admin/spaces");
  await page.locator("a", { hasText: "Arrange the default front page" }).click();
  await expect(page.locator("h1")).toHaveText("Default front page");
  await expect(page.locator(".subnav [aria-current=page]")).toHaveText("Default front page");
  await editorReady(page);
  await expectFits(page);
  const unplaced = withJs()
    ? page.locator(".layout-available")
    : page.locator("section[aria-labelledby='unplaced-heading']");
  await expect(unplaced.locator(`a[href='/c/${slug}']`)).toBeVisible();

  // A section with the space in it reaches visitors' front page.
  await addSection(page, extra);
  await putIn(page, slug, extra, name, true);
  const visitor = await browser.newPage();
  await visitor.goto("/");
  const table = visitor.locator(".space-list", { has: visitor.locator("th a", { hasText: extra }) });
  await expect(table.locator(`a[href='/c/${slug}']`)).toBeVisible();

  // Essential, and back.
  const row = section(page, extra).locator("li", { has: page.locator(`a[href='/c/${slug}']`) });
  await row.locator("button", { hasText: "Make it essential" }).click();
  await expect(row.locator(".flag", { hasText: "Essential" })).toBeVisible();
  await expectFits(page);
  await row.locator("button", { hasText: "Make it not essential" }).click();
  await expect(row.locator(".flag", { hasText: "Essential" })).toHaveCount(0);

  // Applying the default again to the members who follow it.
  await page.locator("button", { hasText: "Apply to members who haven’t edited theirs" }).click();
  await expect(page.locator(".notice-success")).toContainText("Applied");
  await editorReady(page);

  // Deleting the section takes it off visitors' front page again.
  await section(page, extra).locator("button, a", { hasText: "Delete section…" }).click();
  if (!withJs()) {
    await expect(page.locator("h1")).toHaveText(`Delete the ${extra} section?`);
    await expect(page.locator("p", { hasText: "every member who hasn’t arranged their own" })).toBeVisible();
  }
  await page.locator("button", { hasText: "Delete the section" }).click();
  await expect(section(page, extra)).toHaveCount(0);
  await visitor.goto("/");
  await expect(visitor.locator(".space-list th", { hasText: extra })).toHaveCount(0);
  await visitor.close();
});
