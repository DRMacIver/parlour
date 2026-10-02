// Starts Parlour for the browser tests. Playwright runs this as its
// webServer and stops it afterwards.
//
// - Builds the JavaScript islands in ../web (run `npm install` there once)
//   and then the server, which embeds them.
// - Recreates the `parlour_e2e` database on the dev Postgres, fills it
//   with `parlour seed`, and serves it on port E2E_PORT (default 18080),
//   sending email to the dev compose Mailpit.
// - Serves two more forums from empty databases, for the tests of the
//   first-run setup, on the next two ports: one for the tests without
//   JavaScript and one for those with it. Each prints its one-time setup
//   link, which is written to .setup-links/<port>.txt for the tests.
//
//   E2E_POSTGRES_URL  server to use (default: the dev compose Postgres)
//   E2E_PORT          port to serve on (default 18080)
//   E2E_SMTP_PORT     Mailpit's SMTP port (default 1025)

import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import pg from "pg";

const postgres = process.env.E2E_POSTGRES_URL ?? "postgres://parlour:parlour@localhost:54329";
const port = Number(process.env.E2E_PORT ?? "18080");
const setupPorts = [port + 1, port + 2];
const here = (path) => fileURLToPath(new URL(path, import.meta.url));
const manifest = here("../server/Cargo.toml");
const binary = here("../server/target/debug/parlour");
const links = here("./.setup-links");

function run(command, args, options) {
  const result = spawnSync(command, args, { stdio: "inherit", ...options });
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

// The islands first, so that the server embeds them.
run("npm", ["run", "build"], { cwd: here("../web") });
// Check queries against server/.sqlx, not the empty databases below.
run("cargo", ["build", "--quiet", "--manifest-path", manifest], {
  env: { ...process.env, SQLX_OFFLINE: "true" },
});

const databases = ["parlour_e2e", ...setupPorts.map((p) => `parlour_e2e_setup_${p}`)];
const admin = new pg.Client({ connectionString: `${postgres}/postgres` });
await admin.connect();
for (const database of databases) {
  await admin.query(`DROP DATABASE IF EXISTS ${database} WITH (FORCE)`);
  await admin.query(`CREATE DATABASE ${database}`);
}
await admin.end();

const env = (database, servePort) => ({
  ...process.env,
  DATABASE_URL: `${postgres}/${database}?sslmode=disable`,
  PARLOUR_BIND: `127.0.0.1:${servePort}`,
  PARLOUR_BASE_URL: `http://127.0.0.1:${servePort}`,
  // Each test says which address it comes from in X-Forwarded-For, so that
  // tests running side by side don't share rate limits.
  PARLOUR_TRUST_PROXY: "true",
  PARLOUR_SMTP_HOST: "127.0.0.1",
  PARLOUR_SMTP_PORT: process.env.E2E_SMTP_PORT ?? "1025",
  PARLOUR_SMTP_SECURITY: "none",
  PARLOUR_MAIL_FROM: "Parlour e2e <forum@e2e.example>",
  PARLOUR_UPLOADS_DIR: here(`./.uploads/${servePort}`),
  RUST_LOG: process.env.RUST_LOG ?? "warn",
});

run(binary, ["seed"], { env: env("parlour_e2e", port) });

const servers = [];
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    for (const server of servers) {
      server.kill(signal);
    }
    process.exit(0);
  });
}

rmSync(links, { recursive: true, force: true });
mkdirSync(links, { recursive: true });
for (const setupPort of setupPorts) {
  const server = spawn(binary, ["serve"], {
    env: env(`parlour_e2e_setup_${setupPort}`, setupPort),
    stdio: ["ignore", "pipe", "pipe"],
  });
  servers.push(server);
  for (const stream of [server.stdout, server.stderr]) {
    stream.on("data", (chunk) => {
      const text = chunk.toString();
      process.stdout.write(text);
      const link = text.match(/http:\/\/\S+\/setup\?token=[A-Za-z0-9_-]+/);
      if (link) {
        writeFileSync(`${links}/${setupPort}.txt`, link[0]);
      }
    });
  }
}

const main = spawn(binary, ["serve"], { env: env("parlour_e2e", port), stdio: "inherit" });
servers.push(main);
main.on("exit", (code) => {
  for (const server of servers) {
    server.kill("SIGTERM");
  }
  process.exit(code ?? 0);
});
