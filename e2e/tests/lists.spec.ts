// Lists (DESIGN.md §3.3), in a real browser: a member makes a list from
// their settings and puts someone on it, and that someone finds it among
// the lists they are on, with whose it is and who else is on it. Each
// project uses a member of its own as the list's owner (the members whose
// front pages the layout editor's tests arrange: lists don't touch front
// pages), and its own other member to put on the list.

import { expect, test } from "@playwright/test";
import { expectFits, logIn, otherMember, ownAddress } from "./helpers.js";

const OWNERS: Record<string, string> = {
  "no-js-320-light": "alma",
  "no-js-320-dark": "bram",
  "no-js-1280-light": "cleo",
  "no-js-1280-dark": "dara",
  "js-1280": "emil",
  "js-320": "fern",
};

function owner(): string {
  return OWNERS[test.info().project.name] ?? "alma";
}

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

test("a member makes a list, and the person they put on it can see it", async ({ page }) => {
  const name = `Walkers ${Date.now() % 100000}`;
  const other = otherMember();
  await logIn(page, owner());
  await page.goto("/settings");
  await page.locator("#lists a[href='/settings/lists']").click();
  await expect(page.locator("h1")).toHaveText("Your lists");
  // Everyone starts with a Friends list.
  await expect(page.locator("a", { hasText: "Friends" }).first()).toBeVisible();
  await expect(page.getByText("People on this list can see its name and who else is on it.")).toBeVisible();
  await expectFits(page);

  await page.fill("#name", name);
  await page.locator("button", { hasText: "Make the list" }).click();
  await expect(page.locator("h1")).toHaveText(name);
  await expect(page.locator(".notice-success")).toHaveText("The list is ready. Add people to it by username.");
  await page.fill("#username", other);
  await page.locator("button", { hasText: /^Add$/ }).click();
  await expect(page.locator(".notice-success")).toHaveText("Added to the list.");
  await expect(page.locator(".member-list")).toContainText(`@${other}`);
  await expectFits(page);

  await page.locator("form[action='/logout'] button").click();
  await logIn(page, other);
  await page.goto("/settings/lists");
  const entry = page.locator("li", { hasText: name });
  await expect(entry).toBeVisible();
  await entry.locator("a").click();
  await expect(page.locator("h1")).toContainText(name);
  await expect(page.locator(".member-list")).toContainText(`@${other}`);
  // Only the owner changes it.
  await expect(page.locator("#username")).toHaveCount(0);
  await expectFits(page);
});
