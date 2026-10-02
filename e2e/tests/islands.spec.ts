// The JavaScript islands (DESIGN.md §5), which only the projects with
// JavaScript run, at 1280px and 320px: reacting without a reload, the
// composer's preview, images and quoting a selection, the live "N new
// posts" bar, and the notification badge and menu kept up to date as
// things happen. Each must fit the screen. The plain forms each island
// replaces are tested by the other specs, with and without JavaScript.

import { expect, test, type Browser, type Page } from "@playwright/test";
import { expectFits, expectOnScreen, logIn, ownAddress } from "./helpers.js";

// A 2×2 PNG.
const PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFElEQVR4nGP4z8DAAMIM////ZwAAHu8E/KPItPcAAAAASUVORK5CYII=",
  "base64",
);

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

async function loggedIn(browser: Browser, username: string): Promise<Page> {
  const context = await browser.newContext({ ...test.info().project.use });
  const page = await context.newPage();
  await ownAddress(page);
  await logIn(page, username);
  return page;
}

/** Starts a thread of the page's own in Board games, and stays on it. */
async function newThread(page: Page, title: string, body: string) {
  await page.goto("/c/board-games/new");
  await page.fill("#title", title);
  await page.fill("#body", body);
  await page.locator(".composer button[type=submit]:not([name])").click();
  await expect(page.locator("h1")).toHaveText(title);
}

/** Marks the page, so that a reload can be noticed. */
async function mark(page: Page) {
  await page.evaluate(() => {
    (window as unknown as { marked: boolean }).marked = true;
  });
}

/** The number on the notifications badge: 0 when there is none. */
async function badge(page: Page): Promise<number> {
  const badges = page.locator(".topnav [data-count=notifications]");
  return (await badges.count()) === 0 ? 0 : Number(await badges.textContent());
}

async function stillMarked(page: Page): Promise<boolean> {
  return page.evaluate(() => (window as unknown as { marked?: boolean }).marked === true);
}

// Members no other spec or project logs in as, since logins are rate
// limited per account, and the notification tests count what each gets.
const ISLAND_MEMBERS: Record<string, [string, string]> = {
  "js-1280": ["iris", "jude"],
  "js-320": ["uma", "vic"],
};

let AUTHOR: string;
let OTHER: string;

test.beforeEach(() => {
  [AUTHOR, OTHER] = ISLAND_MEMBERS[test.info().project.name] ?? ["iris", "jude"];
});

test("islands load for members, and reactions change without a reload", async ({ page }) => {
  await logIn(page, AUTHOR);
  await page.goto("/c/board-games");
  await page.locator(".row-title a", { hasText: "Cascadia scoring variants" }).first().click();
  await expect(page.locator("[data-island-mounted]").first()).toBeAttached();
  await mark(page);
  const post = page.locator("article.post").nth(1);
  const before = await post.locator(".reactions button[aria-pressed]").count();
  await post.locator(".react-menu summary").click();
  await post.locator(".react-choices button[value='🙏']").click();
  const mine = post.locator(".reactions button[aria-pressed=true]", { hasText: "🙏" });
  await expect(mine).toBeVisible();
  // The menu closes, and the page wasn't reloaded.
  await expect(post.locator(".react-menu")).not.toHaveAttribute("open");
  expect(await stillMarked(page)).toBe(true);
  await mine.click();
  await expect(post.locator(".reactions button[aria-pressed]")).toHaveCount(before);
  expect(await stillMarked(page)).toBe(true);
  await expectFits(page);
});

test("the composer previews in place, adds images and quotes a selection", async ({ page }) => {
  await logIn(page, OTHER);
  const title = `Composer island ${Date.now()}`;
  await page.goto("/c/board-games/new");
  await page.fill("#title", title);
  await page.fill("#body", "A **bold** start.");
  await mark(page);
  await expectFits(page);
  await expectOnScreen(page.locator(".composer-island").first());
  await page.locator(".composer-tab", { hasText: "Preview" }).click();
  await expect(page.locator(".composer-preview strong")).toHaveText("bold");
  expect(await stillMarked(page)).toBe(true);
  await expectFits(page);
  await page.locator(".composer-tab", { hasText: "Write" }).click();

  // An image chosen with the button goes in as Markdown.
  await page.locator(".composer-island input[type=file]").setInputFiles({
    name: "tiny.png",
    mimeType: "image/png",
    buffer: PNG,
  });
  await expect(page.locator("#body")).toHaveValue(/!\[\]\(\/uploads\/[0-9a-f]{64}\.png\)/);
  // And so does one pasted in.
  await page.locator("#body").focus();
  await page.evaluate((bytes) => {
    const data = new DataTransfer();
    data.items.add(new File([new Uint8Array(bytes)], "pasted.png", { type: "image/png" }));
    document.querySelector("#body")!.dispatchEvent(
      new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true }),
    );
  }, [...PNG]);
  await expect(page.locator("#body")).toHaveValue(/(!\[\]\(\/uploads\/[0-9a-f]{64}\.png\)[\s\S]*){2}/);
  await page.locator(".composer button[type=submit]:not([name])").click();
  await expect(page.locator("h1")).toHaveText(title);
  const image = page.locator("article.post").first().locator(".post-body img").first();
  await expect(image).toBeVisible();
  expect(await image.evaluate((img) => (img as HTMLImageElement).naturalWidth)).toBe(2);

  // Selecting part of a post and choosing "quote" quotes just that.
  await page.locator("#reply-body").fill("");
  await page.evaluate(() => {
    const strong = document.querySelector("article.post .post-body strong")!;
    const range = document.createRange();
    range.selectNodeContents(strong);
    const selection = window.getSelection()!;
    selection.removeAllRanges();
    selection.addRange(range);
  });
  await page.locator("article.post").first().locator("a[data-quote]").click();
  await expect(page.locator("#reply-body")).toHaveValue(new RegExp(`^> @${OTHER} wrote:\\n> bold\\n\\n$`));
  await expect(page.locator(".composer-reply-to")).toContainText("In reply to № 1");
  await page.locator("#reply-body").press("End");
  await page.locator("#reply-body").pressSequentially("Indeed.");
  await page.locator("#reply button[type=submit]:not([name])").click();
  const reply = page.locator("article.post").last();
  await expect(reply.locator("blockquote")).toContainText("bold");
  await expect(reply.locator(".reply-to")).toContainText(`@${OTHER}`);
});

test("new posts and notifications arrive while the page is open", async ({ page, browser }) => {
  await logIn(page, AUTHOR);
  const title = `Live island ${Date.now()}`;
  await newThread(page, title, "Who is coming on Thursday?");
  const threadUrl = page.url();
  const unread = await badge(page);
  await mark(page);

  // Someone else replies, quoting the first post.
  const other = await loggedIn(browser, OTHER);
  await other.goto(threadUrl);
  await other.locator("article.post").first().locator("a[data-quote]").click();
  await expect(other.locator("#reply-body")).toHaveValue(/wrote:/);
  await other.locator("#reply-body").press("End");
  await other.locator("#reply-body").pressSequentially("Me!");
  await other.locator("#reply button[type=submit]:not([name])").click();
  await expect(other.locator("article.post").last()).toContainText("Me!");

  // The author's open page says so, without reloading.
  const bar = page.locator(".new-posts-bar");
  await expect(bar).toContainText("1 new post");
  await expectOnScreen(bar);
  // And the notification badge goes up, once the notification is made.
  await expect.poll(() => badge(page), { timeout: 15_000 }).toBeGreaterThan(unread);
  expect(await stillMarked(page)).toBe(true);

  // The menu lists the reply.
  await page.locator(".nav-menu-button").click();
  const menu = page.locator(".notifications-menu");
  await expect(menu).toContainText(title);
  await expectFits(page);
  await expectOnScreen(menu);
  await menu.locator("button", { hasText: "Mark all read" }).click();
  await expect(page.locator(".topnav [data-count=notifications]")).toHaveCount(0);
  await page.keyboard.press("Escape");
  await expect(menu).toHaveCount(0);

  // The bar leads to the new post.
  await bar.locator("a").click();
  await expect(page).toHaveURL(/#p\d+$/);
  await expect(page.locator("article.post").last()).toContainText("Me!");
  await other.context().close();
});

test("visitors' pages load no JavaScript at all", async ({ page }) => {
  const scripts: string[] = [];
  page.on("request", (request) => {
    if (request.resourceType() === "script") {
      scripts.push(request.url());
    }
  });
  for (const path of ["/", "/latest", "/c/board-games", "/search?q=heat", "/users/tree"]) {
    await page.goto(path);
    await expectFits(page);
  }
  await page.goto("/");
  await page.locator("a", { hasText: "Board games" }).first().click();
  await page.locator(".row-title a").first().click();
  await expectFits(page);
  expect(scripts).toEqual([]);
});
