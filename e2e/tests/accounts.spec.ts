// Accounts and invites in a real browser, with and without JavaScript, at
// 320px and 1280px: logging in, the logged-in pages, inviting someone and
// joining with the link, and logging in with an emailed link and code
// (delivered over SMTP to the dev compose Mailpit).

import { expect, test, type Page } from "@playwright/test";
import pg from "pg";
import { expectFits, logIn, member, otherMember, ownAddress } from "./helpers.js";

const MAILPIT = process.env.E2E_MAILPIT_URL ?? "http://127.0.0.1:8025";

/**
 * The first match for `pattern` in the newest email to `to` that arrived
 * after `since` and matches it, from Mailpit. Matching on what is wanted
 * (a reset link, say) rather than taking the newest email means an earlier
 * email that arrived late, or close to `since`, can't be taken for it.
 */
async function emailTo(page: Page, to: string, since: number, pattern: RegExp): Promise<RegExpMatchArray> {
  for (let i = 0; i < 50; i++) {
    const search = await page.request.get(`${MAILPIT}/api/v1/search?query=${encodeURIComponent(`to:"${to}"`)}`);
    const { messages } = await search.json();
    // Newest first. Mailpit's clock is the same as ours, give or take.
    const fresh = (messages ?? []).filter((m: { Created: string }) => Date.parse(m.Created) >= since - 1000);
    for (const { ID } of fresh) {
      const message = await page.request.get(`${MAILPIT}/api/v1/message/${ID}`);
      const match = ((await message.json()).Text as string).match(pattern);
      if (match) {
        return match;
      }
    }
    await page.waitForTimeout(200);
  }
  throw new Error(`no email to ${to} matching ${pattern}`);
}

const LOGIN_CODE = /^\s*(\d{6})\s*$/m;
const LINK = (path: string) => new RegExp(`http://\\S+/${path}/\\S+`);

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

test("an admin sees the site settings and members", async ({ page }) => {
  await logIn(page, "sam");
  await page.goto("/admin/settings");
  await expect(page.locator("#site_name")).toHaveValue("The Tuesday Club");
  await expectFits(page);
  await page.locator(".subnav a[href='/admin/users']").click();
  await expect(page.locator("h1")).toHaveText("Members and roles");
  await expect(page.locator("form[action$='/role']").first()).toBeVisible();
  await expectFits(page);
});

// A long chain of invitations, deeper than the seed data's, added to the
// database directly (once, whichever test gets there first).
const CHAIN_DEPTH = 24;

async function addInviteChain() {
  const postgres = process.env.E2E_POSTGRES_URL ?? "postgres://parlour:parlour@localhost:54329";
  const db = new pg.Client({ connectionString: `${postgres}/parlour_e2e` });
  await db.connect();
  try {
    await db.query("BEGIN");
    await db.query("SELECT pg_advisory_xact_lock(hashtext('e2e invite chain'))");
    const { rows } = await db.query("SELECT 1 FROM users WHERE username = 'chain_01'");
    if (rows.length === 0) {
      let parent = (await db.query("SELECT id FROM users WHERE username = 'sam'")).rows[0].id;
      for (let i = 1; i <= CHAIN_DEPTH; i++) {
        const n = String(i).padStart(2, "0");
        // Some names are long and have nowhere to break.
        const name = i % 5 === 0 ? `Bartholomew-Featherstonehaugh-${n}` : `Chain Member Number ${n}`;
        const inserted = await db.query(
          "INSERT INTO users (username, display_name, email, email_verified_at, role, invited_by_user_id) VALUES ($1, $2, $3, now(), 'user', $4) RETURNING id",
          [`chain_${n}`, name, `chain_${n}@e2e.example`, parent],
        );
        parent = inserted.rows[0].id;
      }
    }
    await db.query("COMMIT");
  } finally {
    await db.end();
  }
}

test("a long invite chain fits the screen", async ({ page }) => {
  await addInviteChain();
  // Logged in, so the page is rendered fresh rather than from the cache,
  // which doesn't know about the members added behind the app's back.
  await logIn(page, member());
  await page.goto("/users/tree");
  const deepest = page.locator(`a[href='/u/chain_${CHAIN_DEPTH}']`);
  await expect(deepest).toBeVisible();
  await expectFits(page);
  // The deepest members say how deep they are.
  await expect(page.locator(".tree-node", { has: deepest }).locator(".tree-level")).toBeVisible();
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
  const since = Date.now();
  await visitor.fill("#username", username);
  await visitor.fill("#display_name", "E2E Newcomer");
  await visitor.fill("#email", `${username}@e2e.example`);
  await visitor.locator("form button[type=submit]").click();
  // They finish joining from the email sent to the address they typed.
  await expect(visitor.locator("h1")).toHaveText("Check your email");
  await expectFits(visitor);
  const join = (await emailTo(visitor, `${username}@e2e.example`, since, LINK("join")))[0];
  await visitor.goto(join);
  await expectFits(visitor);
  await expect(visitor.locator("#username")).toHaveValue(username);
  await visitor.locator("#content form button[type=submit]").click();
  await expect(visitor).toHaveURL(/\/settings\?done=welcome$/);
  await expect(visitor.locator(".nav-user")).toHaveText(username);
  await expect(visitor.locator(".details")).toContainText("Confirmed");

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
  const code = (await emailTo(page, `${username}@tuesday.example`, since, LOGIN_CODE))[1];
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
  const link = (await emailTo(page, `${username}@tuesday.example`, since, LINK("login")))[0];
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

test("a new member joins and sets a password by reset link", async ({ page, browser }) => {
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
  await expect(newcomer.locator("h1")).toHaveText("Check your email");

  // Finish joining from the email, which confirms the address.
  const join = (await emailTo(newcomer, email, since, LINK("join")))[0];
  await newcomer.goto(join);
  await newcomer.locator("#content form button[type=submit]").click();
  await expect(newcomer).toHaveURL(/\/settings\?done=welcome$/);
  await expect(newcomer.locator(".details")).toContainText("Confirmed");

  // Log out, then choose a password through a reset email.
  await newcomer.locator(".nav-form button").click();
  const resetSince = Date.now();
  await newcomer.goto("/reset");
  await expectFits(newcomer);
  await newcomer.fill("#email", email);
  await newcomer.locator("#content form button[type=submit]").click();
  await expect(newcomer.locator("h1")).toHaveText("Check your email");
  const reset = (await emailTo(newcomer, email, resetSince, LINK("reset")))[0];
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
