import { defineConfig, devices } from "@playwright/test";

const port = Number(process.env.E2E_PORT ?? "18080");
const setup = /setup\.spec\.ts$/;
const islands = /islands\.spec\.ts$/;

// Every page must work without JavaScript and down to 320px wide, in both
// colour schemes (DESIGN.md §1, §5). Each project is one combination; one
// more runs with JavaScript on, and with the islands.
const noJs = { javaScriptEnabled: false };
const narrow = { viewport: { width: 320, height: 640 } };
const wide = { viewport: { width: 1280, height: 900 } };

export default defineConfig({
  testDir: "tests",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  reporter: process.env.CI ? "list" : [["list"], ["html", { open: "never" }]],
  use: {
    ...devices["Desktop Chrome"],
    baseURL: `http://127.0.0.1:${port}`,
  },
  projects: [
    // The main flows, against the seeded forum. The islands' own tests
    // need JavaScript, so only the last of these runs them.
    { name: "no-js-320-light", testIgnore: [setup, islands], use: { ...noJs, ...narrow, colorScheme: "light" } },
    { name: "no-js-320-dark", testIgnore: [setup, islands], use: { ...noJs, ...narrow, colorScheme: "dark" } },
    { name: "no-js-1280-light", testIgnore: [setup, islands], use: { ...noJs, ...wide, colorScheme: "light" } },
    { name: "no-js-1280-dark", testIgnore: [setup, islands], use: { ...noJs, ...wide, colorScheme: "dark" } },
    { name: "js-1280", testIgnore: [setup], use: { ...wide, colorScheme: "light" } },
    // Setting up a new forum, which happens once per forum: each of these
    // has an empty forum of its own (see server.mjs).
    {
      name: "setup-no-js",
      testMatch: setup,
      use: { ...noJs, ...narrow, colorScheme: "light", baseURL: `http://127.0.0.1:${port + 1}` },
    },
    {
      name: "setup-js",
      testMatch: setup,
      use: { ...wide, colorScheme: "dark", baseURL: `http://127.0.0.1:${port + 2}` },
    },
  ],
  webServer: {
    command: "node server.mjs",
    url: `http://127.0.0.1:${port}/healthz`,
    // The first run compiles the server.
    timeout: 900_000,
    reuseExistingServer: false,
    stdout: "pipe",
  },
});
