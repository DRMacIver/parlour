// Writing in a real browser, with and without JavaScript, at 320px and
// 1280px: starting a thread with a preview, the quick reply, quoting,
// editing, reactions, quote-forks, deleting, edit history and marking a
// category read. Every page must fit the screen and need no JavaScript.

import { expect, test, type Page } from "@playwright/test";
import { expectFits, logIn, member, ownAddress, preview, withJs } from "./helpers.js";

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

/** The last post on the page that isn't a tombstone. */
function lastPost(page: Page) {
  return page.locator("article.post:not(.post-tombstone)").last();
}

test("a member starts a thread, replies, quotes, edits, reacts and deletes", async ({ page }) => {
  const username = member();
  await logIn(page, username);
  const title = `Heat with five (${test.info().project.name} ${Date.now()})`;

  // New thread, previewed first.
  await page.goto("/c/board-games");
  await expectFits(page);
  await page.locator(".toolbar a", { hasText: "New thread" }).click();
  await expectFits(page);
  await page.fill("#title", title);
  await page.fill("#body", "We played **Heat** with five.\n\n| Game | Players |\n|---|---|\n| Heat | 5 |");
  await preview(page);
  await expect(page.locator(".preview strong")).toHaveText("Heat");
  await expect(page.locator(".preview table")).toBeVisible();
  await expectFits(page);
  await expect(page.locator("#body")).toHaveValue(/We played \*\*Heat\*\*/);
  await page.locator(".composer button[type=submit]:not([name])").click();
  await expect(page.locator("h1")).toHaveText(title);
  await expect(lastPost(page).locator(".post-body strong")).toHaveText("Heat");
  await expectFits(page);
  const threadUrl = page.url();
  // The new thread's long title, as the category's last post on the index
  // and in the thread lists, still fits a narrow screen.
  for (const path of ["/", "/latest", "/c/board-games"]) {
    await page.goto(path);
    await expect(page.getByText(title).first()).toBeVisible();
    await expectFits(page);
  }
  await page.goto(threadUrl);

  // The quick reply at the foot of the thread.
  await page.fill("#reply-body", "And it only took ten minutes to teach.");
  await page.locator("#reply button[type=submit]:not([name])").click();
  await expect(lastPost(page)).toContainText("ten minutes to teach");
  await expect(lastPost(page).locator(".post-number")).toHaveText("№ 2");

  // Quoting the first post records what the reply is to: in the quick
  // reply with the composer island, or on the reply page without it.
  await page.locator("article.post").first().locator(".post-actions a", { hasText: /^quote$/ }).click();
  const box = withJs() ? "#reply-body" : "#body";
  await expect(page.locator(box)).toHaveValue(new RegExp(`^> @${username} wrote:\\n> We played`));
  if (withJs()) {
    await expect(page.locator(".composer-reply-to")).toContainText("In reply to № 1");
  } else {
    await expect(page.locator("input[name=reply_to]")).toBeChecked();
  }
  await expectFits(page);
  const quoted = await page.locator(box).inputValue();
  await page.fill(box, `${quoted}Agreed.`);
  await page.locator(withJs() ? "#reply button[type=submit]:not([name])" : ".composer button[type=submit]:not([name])").click();
  const quoting = lastPost(page);
  await expect(quoting.locator("blockquote")).toBeVisible();
  await expect(quoting.locator(".reply-to")).toContainText(`@${username}`);
  await expect(quoting.locator(".post-body a[href='/u/" + username + "']")).toBeVisible();

  // Editing the reply (within the grace period, so no history).
  await quoting.locator(".post-actions a", { hasText: "edit" }).click();
  await expect(page.locator("#body")).toHaveValue(/Agreed\.$/);
  await page.fill("#body", "Agreed, entirely.");
  await page.locator(".composer button[type=submit]:not([name])").click();
  await expect(lastPost(page)).toContainText("Agreed, entirely.");
  await expect(lastPost(page).locator(".edited")).toHaveCount(0);

  // Reacting, from the menu, and taking it back.
  const first = page.locator("article.post").first();
  await first.locator(".react-menu summary").click();
  await first.locator(".react-choices button[value='👍']").click();
  const thumbs = page.locator("article.post").first().locator(".reactions button[aria-pressed=true]");
  await expect(thumbs).toContainText("1");
  await expectFits(page);
  await thumbs.click();
  await expect(page.locator("article.post").first().locator(".reactions button[aria-pressed=true]")).toHaveCount(0);

  // Deleting the quick reply, after confirming.
  await page.locator("article.post").nth(1).locator(".post-actions a", { hasText: "delete" }).click();
  await expectFits(page);
  await page.locator("button", { hasText: "Delete post" }).click();
  await expect(page.locator(".post-tombstone")).toContainText("Post deleted by its author");
  await expect(page.getByText("ten minutes to teach")).toHaveCount(0);
});

test("a member quotes a post into a new thread", async ({ page }) => {
  await logIn(page, member());
  await page.goto("/c/board-games");
  await page.locator(".row-title a", { hasText: "Cascadia scoring variants" }).first().click();
  const source = page.locator("article.post").first();
  const sourceText = (await source.locator(".post-body p").first().textContent())!;
  await source.locator(".post-actions a", { hasText: "Quote in new thread" }).click();
  const title = `A tangent (${test.info().project.name} ${Date.now()})`;
  // A dialog on the page with the islands, or a page of its own without.
  const prefix = withJs() ? "#fork-" : "#";
  const form = withJs() ? page.locator("dialog[open]") : page.locator("main");
  await expectFits(page);
  await expect(form.locator(".embed .post-body")).toContainText(sourceText);
  await page.fill(`${prefix}title`, title);
  await page.selectOption(`${prefix}category`, "board-games");
  await page.fill(`${prefix}body`, "This deserves its own thread.");
  await form.locator(".composer button[type=submit]:not([name])").click();
  await expect(page.locator("h1")).toHaveText(title);
  await expect(page.locator("figure.embed .post-body")).toContainText(sourceText);
  await expectFits(page);
  // The source post links to the new thread.
  await page.locator("figure.embed a", { hasText: "View in original thread" }).click();
  await expect(page.locator(".fork-link", { hasText: title })).toBeVisible();
});

test("edit history shows what changed", async ({ page }) => {
  await page.goto("/c/board-games");
  await page.locator(".row-title a", { hasText: "What did you play this week? (September)" }).click();
  await page.locator(".pagination a", { hasText: /^3$/ }).first().click();
  const edited = page.locator("article.post", { hasText: "I’m starting to think solo play" });
  await edited.locator("a.edited").click();
  await expect(page.locator("h1")).toHaveText("Edit history");
  await expect(page.locator(".diff ins").first()).toBeVisible();
  await expect(page.locator(".diff del").first()).toBeVisible();
  await expectFits(page);
  await page.locator(".version-list a", { hasText: "Version 1" }).click();
  await expect(page.locator(".diff")).toContainText("I think solo play is its own hobby.");
  await expectFits(page);
});

test("unread counts lead to the first unread post, and categories can be marked read", async ({ page }) => {
  await logIn(page, member());
  await page.goto("/c/books");
  await expectFits(page);
  const unread = page.locator(".thread-list a.unread");
  if ((await unread.count()) > 0) {
    await unread.first().click();
    await expect(page).toHaveURL(/#p\d+$/);
    await page.goto("/c/books");
  }
  await page.locator("button", { hasText: "Mark all read" }).click();
  await expect(page.locator(".thread-list a.unread")).toHaveCount(0);
});

test("visitors see reactions but are asked to log in to write", async ({ page }) => {
  await page.goto("/c/board-games");
  await page.locator(".row-title a", { hasText: "What did you play this week? (September)" }).click();
  await page.locator(".pagination a", { hasText: /^3$/ }).first().click();
  // Visitors are offered only the contact form, to report a post.
  await expect(page.locator(".post-actions a:not([href^='/contact?post='])")).toHaveCount(0);
  await expect(page.locator(".post-actions a[href^='/contact?post=']").first()).toBeVisible();
  await expect(page.locator("a.reaction").first()).toBeVisible();
  await expect(page.locator(".quick-reply a", { hasText: "log in" })).toBeVisible();
  await expectFits(page);
  await page.locator("a.reaction").first().click();
  await expect(page.locator("h1")).toHaveText("Reactions");
  await expectFits(page);
});
