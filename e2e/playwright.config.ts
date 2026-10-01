import { defineConfig, devices } from "@playwright/test";

const port = process.env.E2E_PORT ?? "18080";

// Every page must work without JavaScript and down to 320px wide, in both
// colour schemes (DESIGN.md §1, §5). Each project is one combination; one
// more runs with JavaScript on.
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
    { name: "no-js-320-light", use: { ...noJs, ...narrow, colorScheme: "light" } },
    { name: "no-js-320-dark", use: { ...noJs, ...narrow, colorScheme: "dark" } },
    { name: "no-js-1280-light", use: { ...noJs, ...wide, colorScheme: "light" } },
    { name: "no-js-1280-dark", use: { ...noJs, ...wide, colorScheme: "dark" } },
    { name: "js-1280", use: { ...wide, colorScheme: "light" } },
  ],
  webServer: {
    command: "node server.mjs",
    url: `http://127.0.0.1:${port}/healthz`,
    // The first run compiles the server.
    timeout: 600_000,
    reuseExistingServer: false,
    stdout: "pipe",
  },
});
