// Every reading page renders, fits the viewport with no horizontal scroll,
// and makes no requests to other hosts, in every project in
// playwright.config.ts (with and without JavaScript, 320px and 1280px,
// light and dark). The pages come from the database `parlour seed` fills.

import { expect, test, type Page } from "@playwright/test";

/** Finds the URLs to check by following links from the seeded forum. */
async function discover(page: Page): Promise<Record<string, string>> {
  await page.goto("/c/board-games");
  const pageLink = page.locator(".row-pages a").last();
  const lastPage = (await pageLink.getAttribute("href"))!;
  const longThread = lastPage.split("?")[0];
  const firstThread = (await page.locator(".thread-list .row-title a").first().getAttribute("href"))!;
  // A category with more than one page of threads.
  await page.goto("/");
  let longCategory = "";
  for (const href of await page.locator("a[href^='/c/']").evaluateAll((links) =>
    links.map((link) => link.getAttribute("href")!),
  )) {
    await page.goto(href);
    if ((await page.locator(".pagination a[href*='?page=2']").count()) > 0) {
      longCategory = href;
      break;
    }
  }
  expect(longCategory, "the seed has a category with several pages").not.toBe("");
  await page.goto("/latest");
  const fork = (await page.locator(".row-title a", { hasText: "Do solo modes count" }).first().getAttribute("href"))!;
  return {
    index: "/",
    latest: "/latest",
    "latest page 2": "/latest?page=2",
    category: "/c/board-games",
    "category page 2": `${longCategory}?page=2`,
    thread: firstThread,
    "long thread": longThread,
    "long thread, last page": lastPage,
    "quote-fork": fork,
    search: "/search?q=heat",
    terms: "/terms",
    privacy: "/privacy",
  };
}

let pages: Record<string, string>;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  pages = await discover(page);
  await page.close();
});

async function expectNoHorizontalScroll(page: Page) {
  const { scrollWidth, clientWidth } = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));
  expect(scrollWidth, "page is wider than the viewport").toBeLessThanOrEqual(clientWidth);
}

/** Loads `path`, failing on any request that leaves this server. */
async function visit(page: Page, path: string, status = 200) {
  const host = new URL(test.info().project.use.baseURL!).host;
  const foreign: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.protocol !== "data:" && url.host !== host) {
      foreign.push(request.url());
    }
  });
  const response = await page.goto(path);
  expect(response?.status(), path).toBe(status);
  await page.waitForLoadState("load");
  expect(foreign, "requests to other hosts").toEqual([]);
}

const names = [
  "index",
  "latest",
  "latest page 2",
  "category",
  "category page 2",
  "thread",
  "long thread",
  "long thread, last page",
  "quote-fork",
  "search",
  "terms",
  "privacy",
];

for (const name of names) {
  test(`${name} renders and fits the viewport`, async ({ page }) => {
    await visit(page, pages[name]);
    await expect(page.locator("h1").first()).toBeVisible();
    // The stylesheet applied: the page isn't the browser's default white.
    const background = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    expect(background).not.toBe("rgba(0, 0, 0, 0)");
    expect(background).not.toBe("rgb(255, 255, 255)");
    // Reading pages need no JavaScript at all.
    expect(await page.locator("script").count()).toBe(0);
    await expectNoHorizontalScroll(page);
  });
}

test("the not-found page renders and fits the viewport", async ({ page }) => {
  await visit(page, "/c/no-such-category", 404);
  await expect(page.locator("h1").first()).toBeVisible();
  await expectNoHorizontalScroll(page);
});

test("the author column sits beside posts on wide screens and above them on narrow ones", async ({ page }) => {
  await visit(page, pages["long thread"]);
  const post = page.locator("article.post:not(.post-tombstone)").first();
  const author = (await post.locator(".post-author").boundingBox())!;
  const main = (await post.locator(".post-main").boundingBox())!;
  const width = page.viewportSize()!.width;
  if (width <= 400) {
    expect(author.y + author.height).toBeLessThanOrEqual(main.y + 1);
    // Collapsed to a single line.
    const lineHeight = await post
      .locator(".author-name")
      .evaluate((el) => el.getBoundingClientRect().height);
    expect(author.height).toBeLessThan(lineHeight * 2.5);
  } else {
    expect(author.x + author.width).toBeLessThanOrEqual(main.x + 1);
  }
});

test("pagination links lead to the pages they name", async ({ page }) => {
  await visit(page, pages["long thread"]);
  await page.locator(".pagination a", { hasText: "2" }).first().click();
  await expect(page).toHaveURL(/\?page=2$/);
  await expect(page.locator("article.post").first()).toContainText("№ 51");
});

test("post permalinks land on the post", async ({ page }) => {
  await visit(page, pages["long thread, last page"]);
  const number = page.locator("article.post .post-number").last();
  const id = (await number.getAttribute("href"))!.slice(1);
  await page.goto(`/p/${id.slice(1)}`);
  await expect(page).toHaveURL(new RegExp(`#${id}$`));
  await expect(page.locator(`#${id}`)).toBeVisible();
});
