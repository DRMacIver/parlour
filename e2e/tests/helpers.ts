// Helpers shared by the browser tests.

import { expect, test, type Page } from "@playwright/test";

export const PASSWORD = "tuesday-club";

// Each project logs in as a different seeded member, so that per-account
// rate limits aren't shared between projects running at once.
const MEMBERS: Record<string, string> = {
  "no-js-320-light": "priya",
  "no-js-320-dark": "dev",
  "no-js-1280-light": "lin",
  "no-js-1280-dark": "felix",
  "js-1280": "ana",
};

// Tests that run at the same time as those above, with accounts of their own
// so that their emails can't be mixed up.
const OTHER_MEMBERS: Record<string, string> = {
  "no-js-320-light": "maya",
  "no-js-320-dark": "kit",
  "no-js-1280-light": "ollie",
  "no-js-1280-dark": "jonah",
  "js-1280": "tomasz",
};

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

/** The page fits the viewport with no sideways scrolling, and has no scripts. */
export async function expectFits(page: Page) {
  const { scrollWidth, clientWidth } = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));
  expect(scrollWidth, "page is wider than the viewport").toBeLessThanOrEqual(clientWidth);
  expect(await page.locator("script").count()).toBe(0);
}

export async function logIn(page: Page, username: string) {
  await page.goto("/login");
  await page.fill("#identifier", username);
  await page.fill("#password", PASSWORD);
  await page.locator("form[action='/login'] button[type=submit]").click();
  await expect(page.locator(".nav-user")).toHaveText(username);
}
