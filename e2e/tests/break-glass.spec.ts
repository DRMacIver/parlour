// Breaking glass in a real browser (DESIGN.md §8): a member reports a post
// in a thread only two people can read; the moderator, who can't read it,
// is shown who reported what, where and why, opens it from the queue,
// says why on the confirmation page and reads it; and both people who can
// read it are told on the thread's page, in their notifications and on
// their own access page. Every page must fit the screen and work without
// JavaScript.
//
// Logins, posts and reports are rate limited per account, so each project
// has seeded members of its own for this: an author, the one person they
// let read the thread, and a moderator.

import { expect, test, type Browser } from "@playwright/test";
import { expectFits, logIn, ownAddress } from "./helpers.js";

const PEOPLE: Record<string, { author: string; reader: string; moderator: string; name: string }> = {
  "no-js-320-light": { author: "ines", reader: "omar", moderator: "ugo", name: "Ugo" },
  "no-js-320-dark": { author: "jon", reader: "pia", moderator: "vera", name: "Vera" },
  "no-js-1280-light": { author: "kara", reader: "quinn", moderator: "wim", name: "Wim" },
  "no-js-1280-dark": { author: "lars", reader: "reza", moderator: "xia", name: "Xia" },
  "js-1280": { author: "mina", reader: "sana", moderator: "yuri", name: "Yuri" },
  "js-320": { author: "noor", reader: "tam", moderator: "zoe", name: "Zoe" },
};

function people() {
  return PEOPLE[test.info().project.name] ?? PEOPLE["no-js-320-light"];
}

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

test("a moderator breaks glass on a report, and everyone who can read it is told", async ({ page, browser }) => {
  const { author, reader, moderator: moderatorName, name } = people();
  const stamp = `${test.info().project.name} ${Date.now()}`;
  const title = `Between the two of us (${stamp})`;
  const secret = `The spare key is under the third pot (${stamp})`;
  const details = `Please take this down (${stamp})`;
  const reason = `Checking a report from the queue (${stamp})`;

  // A thread only its author and one other can read.
  await logIn(page, author);
  await page.goto("/c/general/new");
  await page.fill("#title", title);
  await page.fill("#body", secret);
  await page.locator("input[name=read][value=only]").check();
  await page.fill("#read_users", reader);
  await page.locator(".composer button[type=submit]:not([name])").click();
  await expect(page.locator("h1")).toHaveText(title);
  // Without the post's anchor, so that going there again loads the page.
  const threadUrl = page.url().split("#")[0];

  // The other reports it, and is told what the moderators will see.
  const other = await loggedIn(browser, reader);
  await other.goto(threadUrl);
  await other.locator("article.post").first().locator(".post-actions a", { hasText: "report" }).click();
  await expect(other.getByText("everyone who can read it will be told a moderator looked")).toBeVisible();
  await expectFits(other);
  await other.locator("input[name=reason][value=personal]").check();
  await other.fill("#details", details);
  await other.locator("button", { hasText: "Send report" }).click();
  await expect(other.locator("h1")).toHaveText("Thank you");

  // The moderator can't read it. The queue says who reported what, where
  // and why, but not what it says.
  const moderator = await loggedIn(browser, moderatorName);
  expect((await moderator.goto(threadUrl))?.status()).toBe(404);
  await moderator.goto("/mod/queue");
  const item = moderator.locator(".queue-item", { hasText: details });
  await expect(item).toContainText("General chat › a thread you can’t read");
  await expect(item).toContainText(reader);
  await expect(item).toContainText(author);
  await expect(item).not.toContainText(secret);
  await expectFits(moderator);

  // Opening it is breaking glass: one page that says what and who will be
  // told, and asks why.
  await item.locator("a", { hasText: "Open…" }).click();
  await expect(moderator).toHaveURL(/\/mod\/break-glass\?report=\d+$/);
  await expect(moderator.locator("h1")).toHaveText("Break glass?");
  await expect(moderator.locator(".lede")).toContainText("Reading it means breaking glass on this thread.");
  await expect(moderator.getByText("Everyone who can read the thread (2 people) will be told at once")).toBeVisible();
  await expect(moderator.getByText(secret)).toHaveCount(0);
  await expect(moderator.getByText(title)).toHaveCount(0);
  await expectFits(moderator);
  await moderator.fill("#glass_reason", reason);
  await moderator.locator("button", { hasText: "Break glass and read it" }).click();
  await expect(moderator.locator("h1")).toHaveText(title);
  await expect(moderator.getByText(secret)).toBeVisible();
  await expect(moderator.getByText("because you broke glass")).toBeVisible();
  await expectFits(moderator);

  // Both who can read it are told: on the thread, in their notifications,
  // and on their own page of moderators who looked, which says why.
  for (const person of [page, other]) {
    await person.goto(threadUrl);
    await expect(person.locator(".glass-notice")).toContainText(`A moderator (${name}) viewed this thread on`);
    await expectFits(person);
    await person.goto("/notifications");
    await expect(person.locator(".notification", { hasText: title }).first()).toContainText(
      `A moderator (${name}) viewed the thread`,
    );
    await expectFits(person);
    await person.goto("/settings/access");
    await expect(person.locator(".log-entry", { hasText: reason })).toContainText(title);
    await expectFits(person);
  }

  // The public moderation log counts it, without saying what or why.
  const visitor = await browser.newContext({ ...test.info().project.use });
  const anonymous = await visitor.newPage();
  await anonymous.goto("/moderation");
  await expect(anonymous.getByText("Moderators who read private content")).toBeVisible();
  await expect(anonymous.getByText(title)).toHaveCount(0);
  await expect(anonymous.getByText(reason)).toHaveCount(0);
  await expectFits(anonymous);
});
