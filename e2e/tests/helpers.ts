// Helpers shared by the browser tests.

import { expect, test, type Locator, type Page } from "@playwright/test";

export const PASSWORD = "tuesday-club";

// Each project logs in as a different seeded member, so that per-account
// rate limits aren't shared between projects running at once.
const MEMBERS: Record<string, string> = {
  "no-js-320-light": "priya",
  "no-js-320-dark": "dev",
  "no-js-1280-light": "lin",
  "no-js-1280-dark": "felix",
  "js-1280": "ana",
  "js-320": "rosa",
};

// Tests that run at the same time as those above, with accounts of their own
// so that their emails can't be mixed up.
const OTHER_MEMBERS: Record<string, string> = {
  "no-js-320-light": "maya",
  "no-js-320-dark": "kit",
  "no-js-1280-light": "ollie",
  "no-js-1280-dark": "jonah",
  "js-1280": "tomasz",
  "js-320": "theo",
};

// Logins and new conversations are rate limited per account, so the
// projects share out the seeded admins and the members who message each
// other.
const SECOND_HALF = new Set(["no-js-320-dark", "no-js-1280-dark", "js-320"]);

/** An admin to log in as. */
export function admin(): string {
  return SECOND_HALF.has(test.info().project.name) ? "yara" : "sam";
}

/** Two members to message each other: the sender and the recipient. */
export function correspondents(): [string, string] {
  return SECOND_HALF.has(test.info().project.name) ? ["wes", "xan"] : ["hugo", "bea"];
}

export function member(): string {
  return MEMBERS[test.info().project.name] ?? "priya";
}

export function otherMember(): string {
  return OTHER_MEMBERS[test.info().project.name] ?? "maya";
}

/** Gives the page its own client address (the server trusts the proxy header). */
export async function ownAddress(page: Page) {
  const n = () => Math.floor(Math.random() * 250) + 1;
  await page.context().setExtraHTTPHeaders({ "x-forwarded-for": `10.${n()}.${n()}.${n()}` });
}

/**
 * The page fits the viewport with no sideways scrolling, and has no
 * scripts but the islands: none at all for visitors, and for members only
 * the islands' module and their JSON props (DESIGN.md §5).
 */
export async function expectFits(page: Page) {
  const { scrollWidth, clientWidth } = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));
  expect(scrollWidth, "page is wider than the viewport").toBeLessThanOrEqual(clientWidth);
  const scripts = await page.locator("script").evaluateAll((elements) =>
    elements.map((e) => {
      const script = e as HTMLScriptElement;
      return {
        island: script.type === "module" && new URL(script.src, location.href).pathname.startsWith("/static/islands/"),
        props: script.type === "application/json" && script.hasAttribute("data-island-props"),
      };
    }),
  );
  if ((await page.locator(".nav-user").count()) === 0) {
    expect(scripts, "visitors' pages have no scripts").toEqual([]);
  } else {
    for (const script of scripts) {
      expect(script.island || script.props, "only island scripts").toBe(true);
    }
  }
}

/**
 * The element is all on screen: for what sits above the page, such as
 * dialogs, menus and bars, which the page's own width doesn't account for.
 */
export async function expectOnScreen(locator: Locator) {
  const box = (await locator.boundingBox())!;
  const { width, height } = locator.page().viewportSize()!;
  expect(box.x, "starts off the left of the screen").toBeGreaterThanOrEqual(0);
  expect(box.x + box.width, "goes off the right of the screen").toBeLessThanOrEqual(width + 0.5);
  expect(box.width, "wider than the screen").toBeLessThanOrEqual(width);
  expect(box.y, "starts above the screen").toBeGreaterThanOrEqual(0);
  expect(box.height, "taller than the screen").toBeLessThanOrEqual(height);
}

/** Whether this test runs with JavaScript (and so with the islands). */
export function withJs(): boolean {
  return test.info().project.use.javaScriptEnabled !== false;
}

export async function logIn(page: Page, username: string) {
  await page.goto("/login");
  await page.fill("#identifier", username);
  await page.fill("#password", PASSWORD);
  await page.locator("form[action='/login'] button[type=submit]").click();
  await expect(page.locator(".nav-user")).toHaveText(username);
}

/**
 * Previews what is in the composer: in place with the composer island, or
 * by sending the form without it. Either way the preview is in `.preview`.
 */
export async function preview(page: Page) {
  if (withJs()) {
    await page.locator(".composer-tab", { hasText: "Preview" }).click();
  } else {
    await page.locator("button[name=preview]").click();
  }
}
