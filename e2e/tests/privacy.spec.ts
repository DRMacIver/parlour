// Members-only and group-restricted categories in a real browser: who sees
// them and how the index labels them, and an admin making a group, putting
// a member in it and giving it a category of its own.

import { expect, test, type Browser } from "@playwright/test";
import { expectFits, logIn, member, ownAddress } from "./helpers.js";

// The seeded Organisers group (see `ORGANISERS` in server/src/db/seed.rs).
const ORGANISERS = ["sam", "ruth", "priya", "dev"];

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

test("visitors don't see members-only or group categories", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator(".flag-access")).toHaveCount(0);
  await expect(page.locator("a[href='/c/lounge']")).toHaveCount(0);
  for (const path of ["/c/lounge", "/c/organisers"]) {
    const response = await page.goto(path);
    expect(response?.status(), path).toBe(404);
  }
  await expectFits(page);
});

test("the index says who can read each category", async ({ page }) => {
  const username = member();
  await logIn(page, username);
  await page.goto("/");
  const lounge = page.locator(".category-list tr", { has: page.locator("a[href='/c/lounge']") });
  await expect(lounge.locator(".flag-access")).toHaveText("Members only");
  const organisers = page.locator(".category-list tr", { has: page.locator("a[href='/c/organisers']") });
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

test("an admin gives a new group a category of its own", async ({ page, browser }) => {
  const project = test.info().project.name;
  const slug = `e2e-${project}`;
  const groupName = `Testers ${project}`;
  await logIn(page, "sam");

  // The admin pages for categories and groups fit the screen.
  for (const path of ["/admin/categories", "/admin/groups", "/admin/categories/new"]) {
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

  await page.goto("/admin/categories/new");
  await page.fill("#name", `Private ${project}`);
  await page.fill("#slug", slug);
  await page.check("input[name=read_level][value=groups]");
  await page.locator("label.choice", { hasText: groupName }).locator("input[name=read_group]").check();
  await page.locator("form[action='/admin/categories/new'] button[type=submit]").click();
  await expect(page.locator(".notice-success")).toHaveText("Saved.");
  await expect(page.locator(".admin-categories a", { hasText: `Private ${project}` })).toBeVisible();
  await expectFits(page);

  // The member in the group reads it; visitors don't know it is there.
  const reader = await loggedIn(browser, member());
  await reader.goto("/");
  const row = reader.locator(".category-list tr", { has: reader.locator(`a[href='/c/${slug}']`) });
  await expect(row.locator(".flag-access")).toHaveText(`${groupName} group`);
  expect((await reader.goto(`/c/${slug}`))?.status()).toBe(200);
  await reader.context().close();
  const visitor = await browser.newPage();
  expect((await visitor.goto(`/c/${slug}`))?.status()).toBe(404);
  await visitor.close();
});

test("long and many group names wrap instead of widening the page", async ({ page, browser }) => {
  const project = test.info().project.name;
  const reader = member();
  await logIn(page, "sam");

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

  const categories = [
    { slug: `wide-many-${project}`, groups: ids.slice(0, 3) },
    { slug: `wide-long-${project}`, groups: ids.slice(3) },
  ];
  for (const { slug, groups } of categories) {
    await page.goto("/admin/categories/new");
    await page.fill("#name", `Planning ${slug}`);
    await page.fill("#slug", slug);
    await page.check("input[name=read_level][value=groups]");
    for (const id of groups) {
      await page.check(`input[name=read_group][value='${id}']`);
    }
    await page.locator("form[action='/admin/categories/new'] button[type=submit]").click();
    await expect(page.locator(".notice-success")).toHaveText("Saved.");
  }
  await expectFits(page);

  const viewer = await loggedIn(browser, reader);
  await viewer.goto("/");
  for (const { slug } of categories) {
    await expect(viewer.locator(`a[href='/c/${slug}']`)).toBeVisible();
  }
  await expectFits(viewer);
  for (const { slug } of categories) {
    expect((await viewer.goto(`/c/${slug}`))?.status()).toBe(200);
    await expect(viewer.locator("h1 .flag-access")).toContainText("group");
    await expectFits(viewer);
  }
  await viewer.context().close();
});
