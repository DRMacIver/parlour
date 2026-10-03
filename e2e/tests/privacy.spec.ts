// Members-only and group-restricted spaces in a real browser: who sees
// them and how the index labels them, and an admin making a group, putting
// a member in it and giving it a space of its own.

import { expect, test, type Browser, type Page } from "@playwright/test";
import { admin, expectFits, logIn, member, ownAddress } from "./helpers.js";

// The seeded Organisers group (see `ORGANISERS` in server/src/db/seed.rs).
const ORGANISERS = ["sam", "ruth", "priya", "dev"];

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

test("visitors don't see members-only or group spaces", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator(".flag-access")).toHaveCount(0);
  await expect(page.locator("a[href='/c/lounge']")).toHaveCount(0);
  for (const path of ["/c/lounge", "/c/organisers"]) {
    const response = await page.goto(path);
    expect(response?.status(), path).toBe(404);
  }
  await expectFits(page);
});

test("the index says who can read each space", async ({ page }) => {
  const username = member();
  await logIn(page, username);
  await page.goto("/");
  const lounge = page.locator(".space-list tr", { has: page.locator("a[href='/c/lounge']") });
  await expect(lounge.locator(".flag-access")).toHaveText("Members only");
  const organisers = page.locator(".space-list tr", { has: page.locator("a[href='/c/organisers']") });
  if (ORGANISERS.includes(username)) {
    await expect(organisers.locator(".flag-access")).toHaveText("Organisers group");
    await page.goto("/c/organisers");
    await expect(page.locator("h1 .flag-access")).toHaveText("Organisers group");
  } else {
    await expect(organisers).toHaveCount(0);
    const response = await page.goto("/c/organisers");
    expect(response?.status()).toBe(404);
  }
  await expectFits(page);
});

async function loggedIn(browser: Browser, username: string) {
  const context = await browser.newContext({ ...test.info().project.use });
  const page = await context.newPage();
  await ownAddress(page);
  await logIn(page, username);
  return page;
}

test("an admin gives a new group a space of its own", async ({ page, browser }) => {
  const project = test.info().project.name;
  const slug = `e2e-${project}`;
  const groupName = `Testers ${project}`;
  await logIn(page, admin());

  // The admin pages for spaces and groups fit the screen.
  for (const path of ["/admin/spaces", "/admin/groups", "/spaces", "/spaces/new"]) {
    const response = await page.goto(path);
    expect(response?.status(), path).toBe(200);
    await expectFits(page);
  }

  await page.goto("/admin/groups");
  await page.fill("#name", groupName);
  await page.fill("#slug", slug);
  await page.locator("form[action='/admin/groups'] button[type=submit]").click();
  await expect(page.locator("h1")).toHaveText(groupName);
  await page.fill("#username", member());
  await page.locator("form[action$='/members'] button[type=submit]").click();
  await expect(page.locator(".notice-success")).toHaveText("Added to the group.");
  await expect(page.locator(`.admin-members a[href='/u/${member()}']`)).toBeVisible();
  await expectFits(page);

  // Deleting the group asks first, on a page of its own.
  const groupPath = new URL(page.url()).pathname;
  await page.locator(`a[href='${groupPath}/delete']`).click();
  await expect(page.locator("h1")).toHaveText(`Delete ${groupName}?`);
  await expectFits(page);
  await page.locator("form a", { hasText: "Keep it" }).click();
  await expect(page.locator("h1")).toHaveText(groupName);

  // Spaces are made with a name and address; who reads them is set in
  // the space's settings, where the maker lands.
  await page.goto("/spaces/new");
  await page.fill("#name", `Private ${project}`);
  await page.fill("#slug", slug);
  await page.locator("form[action='/spaces/new'] button[type=submit]").click();
  await expect(page.locator(".notice-success")).toContainText("The space is ready");
  await expectFits(page);
  // Each choice's text starts beside its radio or checkbox, wrapping
  // under itself rather than dropping below the control.
  const misplaced = await page.evaluate(() =>
    [...document.querySelectorAll("label.choice")].flatMap((label) => {
      const input = label.querySelector("input")!.getBoundingClientRect();
      const range = document.createRange();
      range.selectNodeContents(label);
      range.setStartAfter(label.querySelector("input")!);
      const first = range.getClientRects()[0];
      return first && first.top < input.bottom && first.left >= input.right ? [] : [label.textContent!.trim()];
    }),
  );
  expect(misplaced).toEqual([]);
  await page.check("input[name=read_level][value=listed]");
  await page.locator("label.choice", { hasText: groupName }).locator("input[name=read_group]").check();
  await page.locator(`form[action='/c/${slug}/settings'] button[type=submit]`).click();
  await expect(page.locator(".notice-success")).toHaveText("Saved.");
  await expectFits(page);
  await page.goto("/admin/spaces");
  await expect(page.locator(".admin-spaces a", { hasText: `Private ${project}` })).toBeVisible();
  await expectFits(page);

  // The member in the group reads it; visitors don't know it is there.
  const reader = await loggedIn(browser, member());
  await reader.goto("/");
  const row = reader.locator(".space-list tr", { has: reader.locator(`a[href='/c/${slug}']`) });
  await expect(row.locator(".flag-access")).toHaveText(`${groupName} group`);
  expect((await reader.goto(`/c/${slug}`))?.status()).toBe(200);
  await reader.context().close();
  const visitor = await browser.newPage();
  expect((await visitor.goto(`/c/${slug}`))?.status()).toBe(404);
  await visitor.close();

  // Long and many group names wrap instead of widening the page.
  await longGroupNamesFit(page, browser);
});

// Signing in as the admin is rate limited per account, so this runs inside
// the test above rather than signing in again.
async function longGroupNamesFit(page: Page, browser: Browser) {
  const project = test.info().project.name;
  const reader = member();

  // Three groups with realistic names, and one with a name at the length limit.
  const names = [
    `Regional coordinators ${project}`,
    `Tournament organisers ${project}`,
    `Treasurers ${project}`,
    `Coordinators of the regional tournaments ${project}`.padEnd(60, "x").slice(0, 60),
  ];
  const ids: string[] = [];
  for (const [n, name] of names.entries()) {
    await page.goto("/admin/groups");
    await page.fill("#name", name);
    await page.fill("#slug", `wide-${n}-${project}`);
    await page.locator("form[action='/admin/groups'] button[type=submit]").click();
    await expect(page.locator("h1")).toHaveText(name);
    ids.push(new URL(page.url()).pathname.split("/").pop()!);
    await page.fill("#username", reader);
    await page.locator("form[action$='/members'] button[type=submit]").click();
    await expect(page.locator(".notice-success")).toHaveText("Added to the group.");
  }

  const spaces = [
    { slug: `wide-many-${project}`, groups: ids.slice(0, 3) },
    { slug: `wide-long-${project}`, groups: ids.slice(3) },
  ];
  for (const { slug, groups } of spaces) {
    await page.goto("/spaces/new");
    await page.fill("#name", `Planning ${slug}`);
    await page.fill("#slug", slug);
    await page.locator("form[action='/spaces/new'] button[type=submit]").click();
    await expect(page.locator(".notice-success")).toContainText("The space is ready");
    await page.check("input[name=read_level][value=listed]");
    for (const id of groups) {
      await page.check(`input[name=read_group][value='${id}']`);
    }
    await page.locator(`form[action='/c/${slug}/settings'] button[type=submit]`).click();
    await expect(page.locator(".notice-success")).toHaveText("Saved.");
  }
  await expectFits(page);

  const viewer = await loggedIn(browser, reader);
  await viewer.goto("/");
  for (const { slug } of spaces) {
    await expect(viewer.locator(`a[href='/c/${slug}']`)).toBeVisible();
  }
  await expectFits(viewer);
  for (const { slug } of spaces) {
    expect((await viewer.goto(`/c/${slug}`))?.status()).toBe(200);
    await expect(viewer.locator("h1 .flag-access")).toContainText("group");
    await expectFits(viewer);
  }
  await viewer.context().close();
}
