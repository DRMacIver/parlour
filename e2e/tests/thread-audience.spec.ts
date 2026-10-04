// Per-thread permissions in a real browser (DESIGN.md §3.4), with and
// without JavaScript: a member starts a thread that only someone they name
// can read, sees its lock and audience by the title and beside the reply
// box, and in thread lists; visitors can't find it; its audience page says
// who can read and reply, and its form widens it back to the space's
// audience, after which nothing extra shows.
//
// Logins and posts are rate limited per account, so each project's author
// is a seeded member of its own who logs in and posts little elsewhere: in
// the projects without JavaScript, the islands' members, who are busy only
// in the two projects with it; in those two, one of each pair of members
// who message each other, who post nothing.

import { expect, test } from "@playwright/test";
import { expectFits, logIn, member, ownAddress } from "./helpers.js";

const AUTHORS: Record<string, string> = {
  "no-js-320-light": "iris",
  "no-js-320-dark": "jude",
  "no-js-1280-light": "uma",
  "no-js-1280-dark": "vic",
  "js-1280": "hugo",
  "js-320": "xan",
};

function author(): string {
  return AUTHORS[test.info().project.name] ?? "iris";
}

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

test("a member restricts a thread to someone they name, then opens it again", async ({ page, browser }) => {
  // The one they name, who needn't log in.
  const other = member();
  await logIn(page, author());
  const title = `Just between us (${test.info().project.name} ${Date.now()})`;

  // Started with "only these people can read it".
  await page.goto("/c/general/new");
  await expect(page.locator("legend", { hasText: "Who can read it" })).toBeVisible();
  await expectFits(page);
  await page.fill("#title", title);
  await page.fill("#body", "A surprise for the club night.");
  await page.locator("input[name=read][value=only]").check();
  await page.fill("#read_users", other);
  await page.locator(".composer button[type=submit]:not([name])").click();
  await expect(page.locator("h1")).toHaveText(title);
  const threadUrl = page.url();

  // A lock and who it is for, by the title and beside the reply box.
  const locks = page.locator(".thread-audience");
  await expect(locks).toHaveCount(2);
  await expect(locks.first()).toContainText("can read this thread.");
  await expect(locks.first().locator("svg")).toBeVisible();
  await expectFits(page);
  // And in the space's list of threads.
  await page.goto("/c/general");
  const row = page.locator(".thread-list tr", { hasText: title });
  await expect(row.locator(".flag-restricted")).toBeVisible();
  await expectFits(page);

  // Visitors can't find it.
  const visitor = await browser.newContext({ javaScriptEnabled: false });
  const visitorPage = await visitor.newPage();
  const response = await visitorPage.goto(threadUrl);
  expect(response?.status()).toBe(404);
  await visitorPage.goto("/c/general");
  await expect(visitorPage.getByText(title)).toHaveCount(0);

  // Who can take part: the space's readers, narrowed to those named.
  await page.goto(threadUrl);
  await locks.first().locator("a", { hasText: "Who can take part" }).click();
  await expect(page).toHaveURL(/\/t\/\d+\/audience$/);
  await expect(page.locator("h1")).toHaveText(`Who can take part in ${title}`);
  await expect(page.locator("#read-heading + p")).toContainText("Only these, of those who can read");
  await expect(page.locator(".member-list").first()).toContainText(`@${other}`);
  await expect(page.locator("#read_users")).toHaveValue(other);
  await expectFits(page);

  // Widening it back to the space's audience.
  await page.locator("input[name=read][value=space]").check();
  await page.locator("form[action$='/audience'] button[type=submit]").click();
  await expect(page.locator(".notice-success")).toHaveText("Saved.");
  await expect(page.locator("#read-heading + p")).toContainText("Everyone who can read");
  await expectFits(page);

  // Now it simply inherits: nothing extra shows, and visitors read it.
  await page.goto(threadUrl);
  await expect(page.locator(".thread-audience")).toHaveCount(0);
  await expect(page.locator(".toolbar a", { hasText: "Who can take part" })).toBeVisible();
  const opened = await visitorPage.goto(threadUrl);
  expect(opened?.status()).toBe(200);
  await expect(visitorPage.locator("h1")).toHaveText(title);
  await visitor.close();
});
