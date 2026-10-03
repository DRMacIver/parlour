// The front page's tabs, and a member arranging their own front page from a
// space's page (DESIGN.md §3.5), in a real browser: Latest offers All and
// the sections; a member follows, unfollows, mutes and unmutes a space with
// plain forms, and a muted space leaves All and the space index.

import { expect, test } from "@playwright/test";
import { expectFits, logIn, otherMember, ownAddress } from "./helpers.js";

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

test("visitors get a tab for each section of the default front page", async ({ page }) => {
  await page.goto("/latest");
  const tabs = page.locator(".tabs a");
  await expect(tabs.first()).toHaveText("All");
  await expect(tabs.first()).toHaveAttribute("aria-current", "page");
  await expect(page.locator(".tabs a", { hasText: "Front page" })).toHaveCount(0);
  await expectFits(page);
  await page.locator(".tabs a", { hasText: "Hobbies" }).click();
  await expect(page.locator("h1")).toHaveText("Hobbies");
  await expect(page.locator(".tabs a[aria-current='page']")).toHaveText("Hobbies");
  await expect(page.locator(".thread-list a[href='/c/board-games']").first()).toBeVisible();
  await expect(page.locator(".thread-list a[href='/c/general']")).toHaveCount(0);
  await expectFits(page);
  // The section's heading on the front page leads to the same tab.
  const href = new URL(page.url()).pathname;
  await page.goto("/");
  await page.locator(`.space-list th a[href='${href}']`).click();
  await expect(page.locator("h1")).toHaveText("Hobbies");
});

test("a member follows, mutes and unmutes a space from its page", async ({ page }) => {
  await logIn(page, otherMember());
  await page.goto("/latest");
  await page.locator(".tabs a", { hasText: "Front page" }).click();
  await expect(page.locator("h1")).toHaveText("Front page");
  await expect(page.locator(".tabs a[aria-current='page']")).toHaveText("Front page");
  await expectFits(page);

  // Board games is on the default front page, and so on theirs, under
  // Hobbies. Unfollowing takes it out of their sections.
  await page.goto("/c/board-games");
  await expect(page.locator(".toolbar")).toContainText("In Hobbies on your front page");
  await expectFits(page);
  await page.locator("form[action='/c/board-games/unfollow'] button").click();
  await expect(page.locator("form[action='/c/board-games/follow'] button")).toBeVisible();
  await page.goto("/");
  const hobbies = page.locator(".space-list", { has: page.locator("th a", { hasText: "Hobbies" }) });
  await expect(hobbies.locator("a[href='/c/board-games']")).toHaveCount(0);
  // Following it again puts it in their first section.
  await page.goto("/c/board-games");
  await page.locator("form[action='/c/board-games/follow'] button").click();
  await expect(page.locator(".toolbar")).toContainText("In Community on your front page");

  // Muting takes it off All and the space index; its own page still works
  // and says so.
  await page.locator("form[action='/c/board-games/mute'] button").click();
  await expect(page.locator(".notice")).toContainText("muted this space");
  await expectFits(page);
  await page.goto("/latest");
  await expect(page.locator(".thread-list a[href='/c/board-games']")).toHaveCount(0);
  await expect(page.locator(".toolbar-note")).toContainText("leaving out the spaces you’ve muted");
  await page.goto("/spaces");
  await expect(page.locator("a[href='/c/board-games']")).toHaveCount(0);
  await page.locator("a", { hasText: "Show the spaces you’ve muted" }).click();
  const row = page.locator(".space-list tr", { has: page.locator("a[href='/c/board-games']") });
  await expect(row.locator(".flag", { hasText: "Muted" })).toBeVisible();
  await expectFits(page);
  await page.goto("/c/board-games");
  await page.locator("form[action='/c/board-games/unmute'] button").click();
  await expect(page.locator("form[action='/c/board-games/mute'] button")).toBeVisible();
  await page.goto("/latest");
  await expect(page.locator(".thread-list a[href='/c/board-games']").first()).toBeVisible();
});
