// Starts Parlour for the browser tests: recreates the `parlour_e2e`
// database on the dev Postgres, fills it with `parlour seed`, and serves it,
// sending email to the dev compose Mailpit. Playwright runs this as its
// webServer and stops it afterwards.
//
//   E2E_POSTGRES_URL  server to use (default: the dev compose Postgres)
//   E2E_PORT          port to serve on (default 18080)
//   E2E_SMTP_PORT     Mailpit's SMTP port (default 1025)

import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import pg from "pg";

const postgres = process.env.E2E_POSTGRES_URL ?? "postgres://parlour:parlour@localhost:54329";
const database = "parlour_e2e";
const port = process.env.E2E_PORT ?? "18080";
const manifest = fileURLToPath(new URL("../server/Cargo.toml", import.meta.url));

const admin = new pg.Client({ connectionString: `${postgres}/postgres` });
await admin.connect();
await admin.query(`DROP DATABASE IF EXISTS ${database} WITH (FORCE)`);
await admin.query(`CREATE DATABASE ${database}`);
await admin.end();

const env = {
  ...process.env,
  DATABASE_URL: `${postgres}/${database}?sslmode=disable`,
  PARLOUR_BIND: `127.0.0.1:${port}`,
  PARLOUR_BASE_URL: `http://127.0.0.1:${port}`,
  // Each test says which address it comes from in X-Forwarded-For, so that
  // tests running side by side don't share rate limits.
  PARLOUR_TRUST_PROXY: "true",
  PARLOUR_SMTP_HOST: "127.0.0.1",
  PARLOUR_SMTP_PORT: process.env.E2E_SMTP_PORT ?? "1025",
  PARLOUR_SMTP_SECURITY: "none",
  PARLOUR_MAIL_FROM: "Parlour e2e <forum@e2e.example>",
  RUST_LOG: process.env.RUST_LOG ?? "warn",
  // Check queries against server/.sqlx, not the empty database above.
  SQLX_OFFLINE: "true",
};
const cargo = (...args) => ["run", "--quiet", "--manifest-path", manifest, "--", ...args];

const seeded = spawnSync("cargo", cargo("seed"), { env, stdio: "inherit" });
if (seeded.status !== 0) {
  process.exit(seeded.status ?? 1);
}

const server = spawn("cargo", cargo("serve"), { env, stdio: "inherit" });
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => server.kill(signal));
}
server.on("exit", (code) => process.exit(code ?? 0));
