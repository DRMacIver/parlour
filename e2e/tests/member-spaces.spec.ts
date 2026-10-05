// A member who isn't staff runs spaces of their own (DESIGN.md §3.1-§3.3),
// in a real browser: they put a friend on their Friends list, open their
// feed to that list alone and post the first thread in it, which the friend
// finds on Latest and on the feed's page while another member is told
// there's no such page; and a member makes a topic space, archives it,
// opens it again and deletes it, being asked first. Every page must fit the
// screen and work without JavaScript.
//
// Logins, posts and new spaces are rate limited per account, so each
// project has seeded members of its own for this: the feed's owner and
// their friend, who have talked before (so that neither is new), and a
// member who is on no list of theirs.

import { expect, test, type Browser } from "@playwright/test";
import { expectFits, logIn, ownAddress } from "./helpers.js";

const PEOPLE: Record<string, { owner: string; friend: string; other: string }> = {
  "no-js-320-light": { owner: "abel", friend: "gail", other: "nico" },
  "no-js-320-dark": { owner: "beth", friend: "hugh", other: "opal" },
  "no-js-1280-light": { owner: "cass", friend: "ivy", other: "pete" },
  "no-js-1280-dark": { owner: "drew", friend: "joel", other: "rhea" },
  "js-1280": { owner: "eden", friend: "kai", other: "saul" },
  "js-320": { owner: "finn", friend: "lena", other: "tess" },
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

test("a member opens their feed to their Friends list alone", async ({ page, browser }) => {
  const { owner, friend, other } = people();
  const stamp = `${test.info().project.name} ${Date.now()}`;
  const title = `Game night write-up (${stamp})`;
  const body = `Nobody flipped the table this time (${stamp})`;

  // The friend goes on the Friends list every member starts with.
  await logIn(page, owner);
  await page.goto("/settings/lists");
  await page.locator(".member-list a", { hasText: "Friends" }).first().click();
  await expect(page.locator("h1")).toHaveText("Friends");
  await page.fill("#username", friend);
  await page.locator("button", { hasText: /^Add$/ }).click();
  await expect(page.locator(".notice-success")).toHaveText("Added to the list.");
  await expectFits(page);

  // The feed's settings: only the owner and the Friends list may read it.
  await page.goto(`/c/${owner}/settings`);
  await page.locator("input[name=read_level][value=listed]").check();
  await page.locator("label.choice", { hasText: "Your Friends" }).first().locator("input[name=read_group]").check();
  await page.locator("form button[type=submit]", { hasText: "Save" }).click();
  await expect(page.locator(".notice-success")).toHaveText("Saved.");
  await expect(page.locator("input[name=read_level][value=listed]")).toBeChecked();
  await expectFits(page);

  // Its first thread.
  await page.goto(`/c/${owner}/new`);
  await page.fill("#title", title);
  await page.fill("#body", body);
  await page.locator(".composer button[type=submit]:not([name])").click();
  await expect(page.locator("h1")).toHaveText(title);
  const threadUrl = page.url().split("#")[0];

  // The friend finds it on Latest and in the feed.
  const friendPage = await loggedIn(browser, friend);
  await friendPage.goto("/latest");
  await expect(friendPage.locator("a", { hasText: title })).toBeVisible();
  await expectFits(friendPage);
  await friendPage.goto(`/c/${owner}`);
  await friendPage.locator("a", { hasText: title }).click();
  await expect(friendPage.locator(".post-body").first()).toContainText(body);
  await expectFits(friendPage);

  // Someone who isn't on the list is told there is no such thing.
  const otherPage = await loggedIn(browser, other);
  expect((await otherPage.goto(threadUrl))?.status()).toBe(404);
  expect((await otherPage.goto(`/c/${owner}`))?.status()).toBe(404);
  await otherPage.goto("/latest");
  await expect(otherPage.locator("a", { hasText: title })).toHaveCount(0);
  await expectFits(otherPage);
});

test("a member makes a topic space, archives it, opens it again and deletes it", async ({ page }) => {
  const { other } = people();
  const n = Date.now() % 100000;
  const name = `Boards ${n}`;
  const slug = `boards-${test.info().project.name}-${n}`;
  await logIn(page, other);
  await page.goto("/spaces");
  await page.locator("a.button", { hasText: "New space" }).click();
  await page.fill("#name", name);
  await page.fill("#slug", slug);
  await page.locator("button", { hasText: "Make the space" }).click();
  await expect(page).toHaveURL(new RegExp(`/c/${slug}/settings`));
  await expect(page.locator(".notice-success")).toContainText("The space is ready.");
  await expectFits(page);

  // Archiving closes it and takes it out of the space index.
  await page.locator("button", { hasText: "Archive this space" }).click();
  await expect(page).toHaveURL(new RegExp(`/c/${slug}$`));
  await expect(page.locator("h1 .flag", { hasText: "Archived" })).toBeVisible();
  await page.goto("/spaces");
  await expect(page.locator(`a[href='/c/${slug}']`)).toHaveCount(0);
  await page.goto("/spaces?archived=1");
  await expect(page.locator(`a[href='/c/${slug}']`)).toBeVisible();

  // And it opens again.
  await page.goto(`/c/${slug}/settings`);
  await page.locator("button", { hasText: "Open it again" }).click();
  await expect(page.locator("h1 .flag", { hasText: "Archived" })).toHaveCount(0);
  await expectFits(page);

  // Deleting it asks first.
  await page.goto(`/c/${slug}/settings`);
  await page.locator("a", { hasText: "Delete this space…" }).click();
  await expect(page.locator("h1")).toHaveText(`Delete ${name}?`);
  await expectFits(page);
  await page.locator("button", { hasText: "Delete the space" }).click();
  await expect(page.locator(".notice-success")).toHaveText("The space was deleted.");
  expect((await page.goto(`/c/${slug}`))?.status()).toBe(404);
});
