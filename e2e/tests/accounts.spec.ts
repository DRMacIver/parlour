// Accounts and invites in a real browser, with and without JavaScript, at
// 320px and 1280px: logging in, the logged-in pages, inviting someone and
// joining with the link, and logging in with an emailed link and code
// (delivered over SMTP to the dev compose Mailpit).

import { expect, test, type Page } from "@playwright/test";

const PASSWORD = "tuesday-club";
const MAILPIT = process.env.E2E_MAILPIT_URL ?? "http://127.0.0.1:8025";

// Each project logs in as a different seeded member, so that per-account
// rate limits aren't shared between projects running at once.
const MEMBERS: Record<string, string> = {
  "no-js-320-light": "priya",
  "no-js-320-dark": "dev",
  "no-js-1280-light": "lin",
  "no-js-1280-dark": "felix",
  "js-1280": "ana",
};

// Tests that run at the same time as those above, with accounts of their own
// so that their emails can't be mixed up.
const OTHER_MEMBERS: Record<string, string> = {
  "no-js-320-light": "maya",
  "no-js-320-dark": "kit",
  "no-js-1280-light": "ollie",
  "no-js-1280-dark": "jonah",
  "js-1280": "tomasz",
};

function member(): string {
  return MEMBERS[test.info().project.name] ?? "priya";
}

function otherMember(): string {
  return OTHER_MEMBERS[test.info().project.name] ?? "maya";
}

/** Gives the page its own client address (the server trusts the proxy header). */
async function ownAddress(page: Page) {
  const n = () => Math.floor(Math.random() * 250) + 1;
  await page.context().setExtraHTTPHeaders({ "x-forwarded-for": `10.${n()}.${n()}.${n()}` });
}

async function expectFits(page: Page) {
  const { scrollWidth, clientWidth } = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));
  expect(scrollWidth, "page is wider than the viewport").toBeLessThanOrEqual(clientWidth);
  expect(await page.locator("script").count()).toBe(0);
}

async function logIn(page: Page, username: string) {
  await page.goto("/login");
  await page.fill("#identifier", username);
  await page.fill("#password", PASSWORD);
  await page.locator("form[action='/login'] button[type=submit]").click();
  await expect(page.locator(".nav-user")).toHaveText(username);
}

/** The newest email to `to` that arrived after `since`, from Mailpit. */
async function emailTo(page: Page, to: string, since: number): Promise<string> {
  for (let i = 0; i < 50; i++) {
    const search = await page.request.get(`${MAILPIT}/api/v1/search?query=${encodeURIComponent(`to:"${to}"`)}`);
    const { messages } = await search.json();
    const fresh = (messages ?? []).filter((m: { Created: string }) => Date.parse(m.Created) >= since - 1000);
    if (fresh.length > 0) {
      const message = await page.request.get(`${MAILPIT}/api/v1/message/${fresh[0].ID}`);
      return (await message.json()).Text as string;
    }
    await page.waitForTimeout(200);
  }
  throw new Error(`no email to ${to}`);
}

test.beforeEach(async ({ page }) => {
  await ownAddress(page);
});

test("the login page renders and fits", async ({ page }) => {
  const response = await page.goto("/login");
  expect(response?.status()).toBe(200);
  expect(response?.headers()["set-cookie"]).toBeUndefined();
  await expect(page.locator("h1")).toHaveText("Log in");
  await expectFits(page);
});

test("a member logs in and their pages fit the screen", async ({ page }) => {
  const username = member();
  await logIn(page, username);
  for (const path of ["/", "/settings", "/invites", "/users/tree", `/u/${username}`]) {
    const response = await page.goto(path);
    expect(response?.status(), path).toBe(200);
    expect(response?.headers()["cache-control"], path).toBe("private, no-store");
    await expect(page.locator(".nav-user")).toHaveText(username);
    await expectFits(page);
  }
  // Members see the members-only section.
  await page.goto("/");
  await expect(page.locator(".category-list th", { hasText: "Members only" })).toBeVisible();
  await page.locator(".nav-form button").click();
  await expect(page.locator(".topnav a[href='/login']")).toBeVisible();
});

test("an admin sees the site settings", async ({ page }) => {
  await logIn(page, "sam");
  await page.goto("/admin/settings");
  await expect(page.locator("#site_name")).toHaveValue("The Tuesday Club");
  await expectFits(page);
});

test("inviting someone, who joins with the link", async ({ page, browser }) => {
  const inviter = member();
  await logIn(page, inviter);
  await page.goto("/invites");
  await page.fill("#note", "e2e test");
  await page.locator("form[action='/invites'] button[type=submit]").click();
  const link = await page.locator(".link-box").inputValue();
  expect(link).toContain("/invite/");
  await expectFits(page);

  // The new member, in a browser of their own.
  const context = await browser.newContext({ ...test.info().project.use });
  const visitor = await context.newPage();
  await ownAddress(visitor);
  const response = await visitor.goto(link);
  expect(response?.headers()["set-cookie"]).toBeUndefined();
  await expectFits(visitor);
  const username = `e2e_${Date.now().toString(36)}${Math.floor(Math.random() * 1000)}`.slice(0, 24);
  await visitor.fill("#username", username);
  await visitor.fill("#display_name", "E2E Newcomer");
  await visitor.fill("#email", `${username}@e2e.example`);
  await visitor.locator("form button[type=submit]").click();
  await expect(visitor).toHaveURL(/\/settings\?done=welcome-verify$/);
  await expect(visitor.locator(".nav-user")).toHaveText(username);

  // They appear under the inviter in the tree and on the inviter's profile.
  await visitor.goto(`/u/${inviter}`);
  await expect(visitor.locator(`.member-list a[href='/u/${username}']`)).toBeVisible();
  await visitor.goto(`/u/${username}`);
  await expect(visitor.locator(`.details a[href='/u/${inviter}']`)).toBeVisible();
  // The invite is used up.
  const again = await visitor.goto(link);
  expect(again?.status()).toBe(200); // logged in: "already a member"
  await context.close();
  const anonymous = await browser.newContext({ ...test.info().project.use });
  const stranger = await anonymous.newPage();
  await ownAddress(stranger);
  expect((await stranger.goto(link))?.status()).toBe(410);
  await anonymous.close();
});

test("logging in with the emailed code", async ({ page }) => {
  const username = member();
  const since = Date.now();
  await page.goto("/login");
  await page.fill("#email", `${username}@tuesday.example`);
  await page.locator("form[action='/login/email'] button[type=submit]").click();
  await expect(page.locator("h1")).toHaveText("Check your email");
  await expectFits(page);
  const text = await emailTo(page, `${username}@tuesday.example`, since);
  const code = text.match(/^\s*(\d{6})\s*$/m)![1];
  await page.fill("#code", code);
  await page.locator("form[action='/login/code'] button[type=submit]").click();
  await expect(page.locator(".nav-user")).toHaveText(username);
});

test("logging in with the emailed link", async ({ page, browser }) => {
  const username = otherMember();
  const since = Date.now();
  await page.goto("/login");
  await page.fill("#email", username);
  await page.locator("form[action='/login/email'] button[type=submit]").click();
  const text = await emailTo(page, `${username}@tuesday.example`, since);
  const link = text.match(/http:\/\/\S+\/login\/\S+/)![0];
  // Opened in another browser, as from an email app.
  const context = await browser.newContext({ ...test.info().project.use });
  const reader = await context.newPage();
  await ownAddress(reader);
  await reader.goto(link);
  await expectFits(reader);
  await reader.locator("form button[type=submit]").click();
  await expect(reader.locator(".nav-user")).toHaveText(username);
  await context.close();
});

test("a new member confirms their address and sets a password by reset link", async ({ page, browser }) => {
  // Invite someone new, so that nothing else uses the account.
  await logIn(page, member());
  await page.goto("/invites");
  await page.locator("form[action='/invites'] button[type=submit]").click();
  const link = await page.locator(".link-box").inputValue();
  const context = await browser.newContext({ ...test.info().project.use });
  const newcomer = await context.newPage();
  await ownAddress(newcomer);
  const username = `e2r_${Date.now().toString(36)}${Math.floor(Math.random() * 1000)}`.slice(0, 24);
  const email = `${username}@e2e.example`;
  const since = Date.now();
  await newcomer.goto(link);
  await newcomer.fill("#username", username);
  await newcomer.fill("#display_name", "Reset Tester");
  await newcomer.fill("#email", email);
  await newcomer.locator("#content form button[type=submit]").click();
  await expect(newcomer.locator(".nav-user")).toHaveText(username);

  // Confirm the address from the email.
  const confirm = (await emailTo(newcomer, email, since)).match(/http:\/\/\S+\/verify\/\S+/)![0];
  await newcomer.goto(confirm);
  await newcomer.locator("#content form button[type=submit]").click();
  await expect(newcomer).toHaveURL(/\/settings\?done=verified$/);
  await expect(newcomer.locator(".details")).toContainText("Confirmed");

  // Log out, then choose a password through a reset email.
  await newcomer.locator(".nav-form button").click();
  const resetSince = Date.now();
  await newcomer.goto("/reset");
  await expectFits(newcomer);
  await newcomer.fill("#email", email);
  await newcomer.locator("#content form button[type=submit]").click();
  await expect(newcomer.locator("h1")).toHaveText("Check your email");
  const reset = (await emailTo(newcomer, email, resetSince)).match(/http:\/\/\S+\/reset\/\S+/)![0];
  await newcomer.goto(reset);
  await expectFits(newcomer);
  await newcomer.fill("#password", "a brand new password");
  await newcomer.fill("#confirm", "a brand new password");
  await newcomer.locator("#content form button[type=submit]").click();
  await expect(newcomer.locator(".nav-user")).toHaveText(username);

  await newcomer.locator(".nav-form button").click();
  await newcomer.goto("/login");
  await newcomer.fill("#identifier", username);
  await newcomer.fill("#password", "a brand new password");
  await newcomer.locator("form[action='/login'] button[type=submit]").click();
  await expect(newcomer.locator(".nav-user")).toHaveText(username);
  await context.close();
});
