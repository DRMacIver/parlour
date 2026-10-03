# Parlour

A minimal, fast, self-hosted web forum in the old-school style: spaces,
flat threads with numbered posts, quote-forks, reactions, direct messages,
invites with a public invite tree, groups, moderation, search and image
uploads. Pages are plain server-rendered HTML that work without JavaScript;
small JavaScript "islands" improve a few forms for logged-in members.
Anonymous visitors are served from an in-memory cache and never get a
cookie.

The design, and the decisions behind it, are in [DESIGN.md](DESIGN.md).
This README is about running Parlour locally for development. Deployment
(the production image, compose file, Caddy and backups) is not covered yet.

## Repository layout

```
server/    The Rust crate (axum, sqlx with Postgres, askama templates)
           migrations/  SQL migrations, run automatically at startup
           templates/   askama templates, compiled into the binary
           static/      parlour.css and the self-hosted fonts
           .sqlx/       sqlx's offline query data (see below)
           tests/       integration and property-based tests
web/       The JavaScript islands: Vite + React + TypeScript
e2e/       Playwright browser tests
dev/       compose.yaml for the development services
design/    UI mockups
research/  background research
```

## What you need

- Rust, through [rustup](https://rustup.rs): the toolchain version is pinned
  in `rust-toolchain.toml` and installed automatically.
- Docker, for Postgres 18 and Mailpit.
- Node.js 26 and npm, for the islands and the browser tests.
- Optionally `sqlx-cli`, to run migrations by hand and to update the
  offline query data:
  `cargo install sqlx-cli --no-default-features --features rustls,postgres`.

## Running it

Start Postgres (on port 54329) and Mailpit (SMTP on 1025, web UI on
http://localhost:8025):

    docker compose -f dev/compose.yaml up -d

Build the islands once (and again whenever you change `web/`; see below):

    cd web && npm install && npm run build && cd ..

Then, in `server/`:

    export DATABASE_URL="postgres://parlour:parlour@localhost:54329/parlour?sslmode=disable"
    cargo run -- seed      # fill the empty database with the sample forum
    cargo run              # serve it at http://localhost:8080

Migrations run automatically whenever `parlour` starts. `cargo run --
help` (or any other unknown argument) prints the commands and environment
variables.

### The sample forum

`parlour seed` creates "The Tuesday Club", a board games and books club
taken from the mockups: 27 members (and one deleted spammer), 3 sections, 8 spaces (one
members-only, one for the Organisers group only), about 70 threads and over
1,500 posts, conversations, reactions, read markers, edit history, and a
moderation queue with reports and a held post.

Every member's password is `tuesday-club`. Some useful accounts:

| Username | Who |
|---|---|
| `sam` | the admin |
| `ruth` | a moderator |
| `priya` | a member in the Organisers group, with unread messages and notifications |
| `jonah` | an ordinary member, not an organiser |
| `wren` | a new member whose first post is waiting for approval |

`parlour seed` refuses to touch a database that already has users or
spaces. `parlour seed --reset` deletes the sample content first, but
only if `parlour seed` created it and nothing has been added since. To
start completely afresh:

    docker compose -f dev/compose.yaml down -v
    docker compose -f dev/compose.yaml up -d
    rm -r uploads          # in server/, or wherever PARLOUR_UPLOADS_DIR points

The uploads go too because they belong to the old database: see "Starting
on the wrong database" below.

A running server keeps serving cached pages from before a seed: restart it
afterwards.

### Setting up an empty forum

Run `cargo run` against an empty database instead of seeding it. The server
logs a one-time link, `http://localhost:8080/setup?token=…`, which walks
through creating the admin account, the email settings (with a test
send) and the first invitations. Then make some spaces (any member can,
from the Spaces page): members have nowhere to post until there are some.
Admin → Spaces arranges them into sections on the index. By default
each new member's first 2 posts are held for a moderator to approve; change
that in Admin → Moderation (0 turns it off).

### Email

To send email to Mailpit, also set

    export PARLOUR_SMTP_HOST=localhost PARLOUR_SMTP_PORT=1025 PARLOUR_SMTP_SECURITY=none
    export PARLOUR_MAIL_FROM="Parlour <forum@localhost>"

and read it at http://localhost:8025. Notification emails go out after the
delay set in /admin/messages (15 minutes by default; set it to 0 to see them
straight away).

### Configuration

Everything else is set in the admin pages. The environment sets:

| Variable | Meaning |
|---|---|
| `DATABASE_URL` | required |
| `PARLOUR_BIND` | address to listen on (default `0.0.0.0:8080`) |
| `PARLOUR_BASE_URL` | the site's address (default `http://localhost:8080`), or `PARLOUR_DOMAIN` for `https://<domain>` |
| `PARLOUR_COOKIE_SECURE` | mark the session cookie `Secure` (default: when the base URL is https) |
| `PARLOUR_TRUST_PROXY` | take client addresses from `X-Forwarded-For` |
| `PARLOUR_UPLOADS_DIR` | where uploaded images are kept (default `./uploads`) |
| `PARLOUR_SMTP_HOST`, `_PORT`, `_SECURITY` (`none`, `starttls`, `tls`), `_USERNAME`, `_PASSWORD`, `PARLOUR_MAIL_FROM` | the SMTP server, unless one is saved in the admin settings |
| `RUST_LOG` | log filter (default `info`) |

### Starting on the wrong database

The first time a forum starts, it gives itself a random id and writes it
both to the database and to `.parlour-forum-id` in the uploads directory.
Every later start checks, before migrating, that the database has that id,
and refuses to start if it doesn't. This stops a forum that comes back up
on an empty database (after a Postgres upgrade or a volume mix-up, say)
from printing a new setup link that anyone who sees the log could use to
take it over, and stops a server pointed at another forum's database. The
error says what to check; to start a new forum on purpose, remove the
file.

## The islands

The islands are in `web/src/islands`: the composer (server-rendered
preview, image paste and upload, quoting), reactions, the notifications
menu with its live badge, the live "N new posts" bar, and the quote-fork
dialog. Each mounts on an element marked `data-island="…"` that holds a
plain HTML form that already works, and reads its props from the JSON
`<script type="application/json" data-island-props>` element after it.
They are loaded only on pages for logged-in members.

`npm run build` in `web/` writes `web/dist`. The server embeds that
directory (with `rust-embed`) and serves it at `/static/islands/`, finding
the hashed file names in Vite's manifest:

- In debug builds the files are read from `web/dist` at run time, so after
  `npm run build` (or with `npm run dev`, which rebuilds on every change) a
  page reload picks them up.
- Release builds include the files in the binary. `server/build.rs` makes
  cargo rebuild when `web/dist` changes, so build the islands first.
- Without a build, the server still compiles and works: pages just have no
  islands.

## Tests

Server tests run against the dev Postgres; each test gets a fresh database:

    cd server
    export DATABASE_URL="postgres://parlour:parlour@localhost:54329/parlour?sslmode=disable"
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

`tests/properties.rs` holds the Hegel property-based tests of the access
layer, the page cache and every page's visibility. They run random forum
histories and render whole forums over HTTP, so how many histories they
try is set in `server/hegel.toml`: a quick pass locally (a minute or two;
most of it is query round trips to Postgres in Docker) and a thorough one
on CI, where Hegel picks its `ci` profile automatically. To run the
thorough pass locally:

    HEGEL_DEFAULT_PROFILE=ci cargo test

or set any number of histories with `HEGEL_TEST_CASES`. The islands have unit tests (`cd web && npm test`), and the browser
tests are described in [e2e/README.md](e2e/README.md):

    cd e2e && npm install && npx playwright install chromium && npm test

### sqlx offline data

Queries are checked at compile time. By default (set in `.cargo/config.toml`)
they're checked against the committed offline data in `server/.sqlx`, so the
server builds whatever `DATABASE_URL` points at, including a brand-new empty
database: the tables only appear when the server starts and migrates it.

After changing a query or adding a migration, check the queries against the
dev database and update the offline data:

    cd server
    sqlx migrate run
    SQLX_OFFLINE=false cargo check --all-targets
    cargo sqlx prepare -- --all-targets

Postgres works out whether a column can be null partly from the query plan,
which depends on what is in the database, so mark nullability explicitly
(`AS "name!"` / `AS "name?"`) on columns from joins and other places where
it could go either way. `python3 dev/check_builds.py` checks that the build
works against an empty database, and that live checking agrees between a
migrated empty database and a seeded one. CI runs it, and generates and
checks the offline data against a freshly migrated database.

## Continuous integration

`.github/workflows/ci.yml` runs formatting, clippy, the offline-data check,
`cargo test` and the islands' build and tests against a Postgres 18
service, and the browser tests with Postgres and Mailpit services.
