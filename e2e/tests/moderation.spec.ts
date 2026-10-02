// Moderation in a real browser (DESIGN.md §8; ModQueue.dc.html): the
// moderators' pages, reporting a post and hiding it from the queue, the
// contact form for visitors, and the public moderation log. Every page must
// fit the screen and work without JavaScript.

import { expect, test, type Browser } from "@playwright/test";
import { expectFits, logIn, ownAddress } from "./helpers.js";

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

async function loggedIn(browser: Browser, username: string) {
  const context = await browser.newContext({ ...test.info().project.use });
  const page = await context.newPage();
  await ownAddress(page);
  await logIn(page, username);
  return page;
}

// Logging in is rate limited per account, and every project runs these at
// once, so each test logs in as few accounts as it can: members the other
// suites don't use, and Ruth once.
test("a member reports a post, and a moderator works through the queue", async ({ page, browser }) => {
  const project = test.info().project.name;
  const secret = `My phone number is 0113 ${Date.now() % 1_000_000} (${project})`;
  await logIn(page, "nell");
  await page.goto("/c/general");
  await page.locator(".toolbar a", { hasText: "New thread" }).click();
  await page.fill("#title", `Lift share (${project} ${Date.now()})`);
  await page.fill("#body", secret);
  await page.locator(".composer button[type=submit]:not([name])").click();
  await expect(page.getByText(secret)).toBeVisible();
  const threadUrl = page.url();

  // Someone else reports it. Members can't reach the moderators' pages.
  const reporter = await loggedIn(browser, "otto");
  await expect(reporter.locator("nav.topnav a", { hasText: "Moderation" })).toHaveCount(0);
  const response = await reporter.goto("/mod/queue");
  expect(response?.status()).toBe(404);
  await reporter.goto(threadUrl);
  await reporter.locator("article.post").first().locator(".post-actions a", { hasText: "report" }).click();
  await expectFits(reporter);
  await reporter.locator("input[name=reason][value=personal]").check();
  await reporter.fill("#details", "Their own number, but still");
  await reporter.locator("button", { hasText: "Send report" }).click();
  await expect(reporter.locator("h1")).toHaveText("Thank you");

  // The moderator's queue has the seeded items and this one.
  const moderator = await loggedIn(browser, "ruth");
  await moderator.locator("nav.topnav a", { hasText: "Moderation" }).click();
  await expect(moderator).toHaveURL(/\/mod\/queue$/);
  for (const text of ["Best replica watches, cheap", "Report on a message", "Held for approval", "Hello from Otley"]) {
    await expect(moderator.getByText(text).first()).toBeVisible();
  }
  await expectFits(moderator);
  const item = moderator.locator(".queue-item", { hasText: secret });
  await expect(item).toContainText("Their own number, but still");
  await item.locator("button", { hasText: "Hide post" }).click();
  await expect(moderator.getByText("Done.")).toBeVisible();
  await expect(moderator.locator(".queue-item", { hasText: secret })).toHaveCount(0);

  // A reported message's conversation is open to moderators.
  await moderator.locator(".queue-item", { hasText: "Report on a message" }).locator(".queue-where a").click();
  await expect(moderator.getByText("one of its messages was reported")).toBeVisible();
  await expect(moderator.locator("#reply-body")).toHaveCount(0);
  await expectFits(moderator);
  for (const path of ["/mod/log", "/mod/users", "/mod/users/hugo", "/moderation"]) {
    await moderator.goto(path);
    await expectFits(moderator);
  }
  await moderator.goto("/mod/users/hugo");
  await expect(moderator.getByText("Talk to him before anything else")).toBeVisible();

  // Visitors see a tombstone; the moderator still sees it, marked.
  const visitor = await browser.newContext({ ...test.info().project.use });
  const anonymous = await visitor.newPage();
  await anonymous.goto(threadUrl);
  await expect(anonymous.getByText("Post hidden by a moderator")).toBeVisible();
  await expect(anonymous.getByText(secret)).toHaveCount(0);
  await expectFits(anonymous);
  await moderator.goto(threadUrl);
  await expect(moderator.getByText(secret)).toBeVisible();
  await expect(moderator.locator(".flag-hidden").first()).toBeVisible();
  await expectFits(moderator);

  // The author is told, and can contest it. Every project hides one of Nell's
  // posts at once, and their notices read alike, so only one
  // project contests one.
  await page.goto("/notifications");
  await expect(page.getByText("A moderator hid your post № 1 in a thread").first()).toBeVisible();
  if (project === "no-js-320-light") {
    await page.locator("a", { hasText: "Contest this" }).first().click();
    await expectFits(page);
    await page.fill("#details", "It's my own number and I want the lift");
    await page.locator("form button[type=submit]", { hasText: "Send" }).click();
    await expect(page.locator("h1")).toHaveText("Sent to the moderators");
  }

  // The public log says what was done, without the post.
  await anonymous.goto("/moderation");
  await expect(anonymous.locator(".log-entry", { hasText: "Lift share" }).first()).toBeVisible();
  await expect(anonymous.getByText(secret)).toHaveCount(0);
  await expectFits(anonymous);
});

test("visitors can write to the moderators", async ({ page }) => {
  await page.goto("/");
  await page.locator("footer a", { hasText: "Contact the moderators" }).click();
  await expectFits(page);
  await page.fill("#email", "worried@example.org");
  await page.fill("#message", `Someone is spamming the Games category (${test.info().project.name})`);
  await page.locator("button", { hasText: "Send to the moderators" }).click();
  await expect(page.getByText("Your message has gone to the moderators")).toBeVisible();
  await expectFits(page);
  const cookies = await page.context().cookies();
  expect(cookies).toHaveLength(0);
});
