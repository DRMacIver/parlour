// Feeds (DESIGN.md §3.2), in a real browser: every member has one from the
// start, which their profile leads to and their own front page shows,
// waiting for its first thread, and which they run from its settings.
// Nothing is posted, so that the feed stays out of everyone else's sight
// and other tests' pages don't change. Each project uses a member of its
// own (those whose front pages the layout editor's tests arrange, which
// keep the Feeds section where the feed is).

import { expect, test } from "@playwright/test";
import { expectFits, logIn, ownAddress } from "./helpers.js";

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

test("a member finds their own feed and where to post in it", async ({ page }) => {
  const me = owner();
  await logIn(page, me);
  await page.goto(`/u/${me}`);
  await page.locator(`.details a[href='/c/${me}']`).click();
  await expect(page).toHaveURL(new RegExp(`/c/${me}$`));
  await expect(page.locator(".thread-meta")).toHaveText("Your feed.");
  await expect(page.locator(".empty")).toContainText("Your feed joins the list of spaces");
  await expect(page.locator(`a[href='/c/${me}/new']`)).toBeVisible();
  await expectFits(page);

  // It is on their own front page from the start, but not in the index.
  await page.goto("/");
  await expect(page.locator(`a[href='/c/${me}']`).first()).toBeVisible();
  await page.goto("/spaces?kind=feed");
  await expect(page.locator(`a[href='/c/${me}']`)).toHaveCount(0);
  await expectFits(page);

  // They run it: its address is theirs, and they can't hand it over.
  await page.goto(`/c/${me}/settings`);
  await expect(page.getByText(`Address: /c/${me}. A feed is always at its owner’s username.`)).toBeVisible();
  await expect(page.getByText("You always run your feed.")).toBeVisible();
  // Nor leave reading, replying or starting threads to staff alone: they
  // always can.
  for (const verb of ["read", "reply", "start", "administer"]) {
    await expect(page.locator(`input[name='${verb}_level'][value='staff']`)).toHaveCount(0);
  }
  await expect(page.getByText("You always can.")).toHaveCount(3);
  await expect(page.locator("form[action$='/delete']")).toHaveCount(0);
  await expectFits(page);
});
