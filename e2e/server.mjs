// Starts Parlour for the browser tests: recreates the `parlour_e2e`
// database on the dev Postgres, fills it with `parlour seed`, and serves it.
// Playwright runs this as its webServer and stops it afterwards.
//
//   E2E_POSTGRES_URL  server to use (default: the dev compose Postgres)
//   E2E_PORT          port to serve on (default 18080)

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
