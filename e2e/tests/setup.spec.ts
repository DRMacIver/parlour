// A new forum, from the first run to its first conversation, in a real
// browser: the setup wizard (admin account, email with a test send through
// Mailpit, the first invitations), a space, a newcomer joining with the
// invitation link, posting, a reply quoting them, and reactions. It runs
// twice, without JavaScript at 320px and with it at 1280px, each against
// an empty forum of its own (see server.mjs).

import { readFileSync } from "node:fs";
import { expect, test, type Page } from "@playwright/test";
import { expectFits, ownAddress, withJs } from "./helpers.js";

const MAILPIT = process.env.E2E_MAILPIT_URL ?? "http://127.0.0.1:8025";

/** The setup link the forum printed when it started. */
async function setupLink(): Promise<string> {
  const port = new URL(test.info().project.use.baseURL!).port;
  for (let i = 0; i < 300; i++) {
    try {
      return readFileSync(new URL(`../.setup-links/${port}.txt`, import.meta.url), "utf8").trim();
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  }
  throw new Error(`no setup link for port ${port}`);
}

async function emailTo(page: Page, to: string, since: number, pattern: RegExp): Promise<RegExpMatchArray> {
  for (let i = 0; i < 50; i++) {
    const search = await page.request.get(`${MAILPIT}/api/v1/search?query=${encodeURIComponent(`to:"${to}"`)}`);
    const { messages } = await search.json();
    for (const { ID, Created } of messages ?? []) {
      if (Date.parse(Created) < since - 1000) {
        continue;
      }
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

test("a new forum is set up, and its first members talk", async ({ page, browser }) => {
  test.setTimeout(120_000);
  await ownAddress(page);
  const tag = `${Date.now().toString(36)}${test.info().project.name === "setup-js" ? "j" : "n"}`;
  const adminEmail = `admin_${tag}@e2e.example`;

  // 1. The admin account and the forum's name.
  await page.goto(await setupLink());
  await expect(page.locator("h1")).toHaveText("Set up your forum");
  await expectFits(page);
  await page.fill("#site_name", "The Thursday Club");
  await page.fill("#username", "founder");
  await page.fill("#display_name", "Fran Founder");
  await page.fill("#email", adminEmail);
  await page.fill("#password", "a long admin password");
  await page.fill("#confirm", "a long admin password");
  await page.locator("form[action='/setup'] button[type=submit]").click();

  // 2. Email: the settings come from the environment (Mailpit); a test
  // email arrives, then they are saved.
  await expect(page.locator("h1")).toHaveText("Set up email");
  await expectFits(page);
  const since = Date.now();
  await page.locator("button[value=test]").click();
  await expect(page.locator(".notice-success")).toBeVisible();
  await emailTo(page, adminEmail, since, /test/i);
  await page.locator("button[value=save]").click();

  // 3. The first invitations: a link for two people.
  await expect(page.locator("h1")).toHaveText("Invite the first members");
  await page.fill("#uses", "2");
  await page.locator("form[action='/setup/invites'] button[type=submit]").click();
  const link = await page.locator(".created-invites .link-box").inputValue();
  expect(link).toContain("/invite/");
  await expectFits(page);
  await page.locator("a", { hasText: "Finish: go to the forum" }).click();
  await expect(page.locator(".site-name")).toHaveText("The Thursday Club");
  await expect(page.locator(".nav-user")).toHaveText("founder");

  // Setup made Announcements and General, on the front page for everyone.
  await expect(page.locator(".space-list a[href='/c/announcements']")).toBeVisible();
  await expect(page.locator(".space-list a[href='/c/general']")).toBeVisible();
  // They are for members: a visitor gets the welcome and no spaces.
  const visitorContext = await browser.newContext({ ...test.info().project.use });
  const visitor = await visitorContext.newPage();
  await ownAddress(visitor);
  await visitor.goto("/");
  await expect(visitor.locator(".welcome")).toContainText("This forum is for its members.");
  await expect(visitor.locator(".welcome a[href='/login']")).toBeVisible();
  await expect(visitor.locator(".space-list")).toHaveCount(0);
  await expect(visitor.locator("#latest-heading")).toHaveCount(0);
  await expectFits(visitor);
  await visitorContext.close();
  // General's settings are the admin's to change; a save keeps it as it is.
  await page.goto("/c/general/settings");
  await expect(page.locator("input[name=essential]")).toBeChecked();
  await page.locator("form[action='/c/general/settings'] button[type=submit]").click();
  await expect(page.locator(".notice-success")).toHaveText("Saved.");

  // A newcomer joins with the link, in a browser of their own.
  const context = await browser.newContext({ ...test.info().project.use });
  const newcomer = await context.newPage();
  await ownAddress(newcomer);
  await newcomer.goto(link);
  await expectFits(newcomer);
  const username = `new_${tag}`.slice(0, 24);
  const joinSince = Date.now();
  await newcomer.fill("#username", username);
  await newcomer.fill("#display_name", "Nina Newcomer");
  await newcomer.fill("#email", `${username}@e2e.example`);
  await newcomer.locator("#content form button[type=submit]").click();
  await expect(newcomer.locator("h1")).toHaveText("Check your email");
  const join = (await emailTo(newcomer, `${username}@e2e.example`, joinSince, /http:\/\/\S+\/join\/\S+/))[0];
  await newcomer.goto(join);
  await newcomer.locator("#content form button[type=submit]").click();
  await expect(newcomer.locator(".nav-user")).toHaveText(username);

  // They fill in their post-info column, then start a thread.
  await newcomer.goto("/settings");
  await newcomer.fill("#status_line", "Here for the biscuits");
  await newcomer.locator("form[action='/settings/profile'] button[type=submit]").click();
  await expect(newcomer.locator(".notice")).toContainText("Your profile has been saved.");
  await newcomer.goto("/c/general");
  await newcomer.locator(".toolbar a", { hasText: "New thread" }).click();
  await newcomer.fill("#title", "Hello, Thursday");
  await newcomer.fill("#body", "Glad to be here. **Biscuits** on me.");
  await newcomer.locator(".composer button[type=submit]:not([name])").click();
  // New members' first posts wait for a moderator.
  await expect(newcomer.locator("h1")).toHaveText("Waiting for approval");
  await expectFits(newcomer);

  // The founder approves it from the queue.
  await page.goto("/mod/queue");
  await expect(page.locator(".queue")).toContainText("Hello, Thursday");
  const held = page.locator(".queue form[action$='/approve']").first();
  await held.locator("input[name=reason]").fill("Welcome aboard");
  await held.locator("button", { hasText: "Approve" }).click();
  await page.goto("/c/general");
  await page.locator(".row-title a", { hasText: "Hello, Thursday" }).click();
  await expect(page.locator("article.post").first()).toContainText("Here for the biscuits");

  // The founder quotes them in a reply.
  if (withJs()) {
    await page.locator("article.post").first().locator("a[data-quote]").click();
    await expect(page.locator("#reply-body")).toHaveValue(/^> @new_.* wrote:\n> Glad to be here/);
    await expect(page.locator(".composer-reply-to")).toContainText("In reply to № 1");
    await page.locator("#reply-body").press("End");
    await page.locator("#reply-body").pressSequentially("Welcome!");
    await page.locator("#reply button[type=submit]:not([name])").click();
  } else {
    await page.locator("article.post").first().locator(".post-actions a", { hasText: /^quote$/ }).click();
    await expect(page.locator("#body")).toHaveValue(/^> @new_.* wrote:\n> Glad to be here/);
    const quoted = await page.locator("#body").inputValue();
    await page.fill("#body", `${quoted}Welcome!`);
    await page.locator(".composer button[type=submit]:not([name])").click();
  }
  const reply = page.locator("article.post").last();
  await expect(reply).toContainText("Welcome!");
  await expect(reply.locator(".reply-to")).toContainText(`@${username}`);
  await expectFits(page);

  // The newcomer reacts to the welcome.
  await newcomer.goto(page.url());
  const welcome = newcomer.locator("article.post").last();
  await welcome.locator(".react-menu summary").click();
  await welcome.locator(".react-choices button").first().click();
  await expect(newcomer.locator("article.post").last().locator(".reactions button[aria-pressed=true]")).toContainText("1");
  await context.close();
});
