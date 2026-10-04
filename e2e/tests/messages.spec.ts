// Direct messages and notifications in a real browser, with and without
// JavaScript, at 320px and 1280px: starting a conversation with a preview,
// the recipient being notified, following the notification, replying,
// turning its notifications off and on, subscribing to a thread, and the
// settings and profile controls around messaging. Every page must fit the
// screen and need no JavaScript.

import { expect, test, type Page } from "@playwright/test";
import { correspondents, expectFits, logIn, ownAddress, preview } from "./helpers.js";

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

async function logOut(page: Page) {
  await page.locator("form[action='/logout'] button").click();
  await expect(page.locator(".topnav a[href='/login']")).toBeVisible();
}

test("members message each other and are notified", async ({ page }) => {
  // Accounts no other test logs in with: logins are rate limited per
  // account, and the other tests already log in as member() and
  // otherMember() often. Each project's conversation has its own title.
  const [sender, recipient] = correspondents();
  const title = `Brass on Tuesday (${test.info().project.name} ${Date.now()})`;

  await logIn(page, sender);
  await page.goto("/messages");
  await expectFits(page);
  await page.locator(".page-head a", { hasText: "New message" }).click();
  await expectFits(page);
  await page.fill("#to", recipient);
  await page.fill("#title", title);
  await page.fill("#body", "Are you **in**?");
  await preview(page);
  await expect(page.locator(".preview strong")).toHaveText("in");
  await expectFits(page);
  await page.locator("form[action='/messages/new'] button[type=submit]:not([name])").click();
  await expect(page.locator("#conversation-heading")).toContainText(title);
  await expect(page.locator("article.message").last()).toContainText("Are you in?");
  await expectFits(page);
  await expect(page.locator(".conversation-list")).toContainText(title);
  await logOut(page);

  // The recipient hears about it, once the notification job has run.
  await logIn(page, recipient);
  await expect(async () => {
    await page.goto("/notifications");
    await expect(page.locator(".notification-list")).toContainText(title);
  }).toPass({ timeout: 15_000 });
  await expect(page.locator(".topnav [data-count=notifications]")).toBeVisible();
  await expectFits(page);
  await page.locator(".notification a", { hasText: title }).first().click();
  await expect(page).toHaveURL(/\/messages\/\d+(\?page=\d+)?#p\d+$/);
  await expect(page.locator("article.message").last()).toContainText("Are you in?");
  await expectFits(page);

  // Replying, and turning the conversation's notifications off and on.
  await page.fill("#reply-body", "Count me in.");
  await page.locator("#reply button[type=submit]:not([name])").click();
  await expect(page.locator("article.message").last()).toContainText("Count me in.");
  await page.locator(".conversation-actions button", { hasText: "Turn off notifications" }).click();
  await expect(page.locator(".conversation-actions button", { hasText: "Turn on notifications" })).toBeVisible();
  await page.locator(".conversation-actions button", { hasText: "Turn on notifications" }).click();
  await expect(page.locator(".conversation-actions button", { hasText: "Turn off notifications" })).toBeVisible();
  await page.goto("/messages");
  await expect(page.locator(".conversation-list")).toContainText(title);
  await expectFits(page);

  // Who can message them, and their email notifications, in settings.
  await page.goto("/settings");
  await expect(page.locator("#messages input[name=dm_policy]")).toHaveCount(3);
  await expect(page.locator("#email input[type=checkbox]")).toHaveCount(4);
  await expect(page.locator("#email input[name=subscriptions]")).toBeVisible();
  await expectFits(page);

  // The sender's profile offers a message, and blocking asks first.
  await page.goto(`/u/${sender}`);
  await expect(page.locator(".profile-actions a", { hasText: "Send a message" })).toBeVisible();
  await page.locator(".profile-actions a", { hasText: "Block" }).click();
  await expect(page.locator("h1")).toContainText("Block");
  await expectFits(page);
  await page.locator("main a", { hasText: "Cancel" }).click();
  await expect(page).toHaveURL(new RegExp(`/u/${sender}$`));

  // Subscribing to a thread from its page; nothing subscribes anyone to a
  // space. (In the same test, so as not to log in again: logins are rate
  // limited per account.)
  await page.goto("/c/board-games");
  await expect(page.locator("form[action$='/subscribe'], form[action$='/watch']")).toHaveCount(0);
  await expectFits(page);
  await page.locator(".list .row-title a:not(.unread)").first().click();
  const toggle = page.locator(".toolbar form.subscription button");
  const before = (await toggle.textContent())?.trim() ?? "";
  expect(["Subscribe", "Unsubscribe"]).toContain(before);
  const after = before === "Subscribe" ? "Unsubscribe" : "Subscribe";
  await toggle.click();
  await expect(toggle).toHaveText(after);
  await expectFits(page);
  await toggle.click();
  await expect(toggle).toHaveText(before);
});

test("visitors are asked to log in for messages and notifications", async ({ page }) => {
  for (const path of ["/messages", "/notifications"]) {
    await page.goto(path);
    await expect(page).toHaveURL(/\/login\?next=/);
  }
});
