# Browser tests

Playwright tests for Parlour's pages and main flows (DESIGN.md §12). They
check that every page renders without JavaScript, fits a 320px screen with
no horizontal scroll, works in light and dark mode, and makes no requests to
other hosts; and they walk through the main flows:

- setting up a new forum (the `/setup` wizard, with a test email), inviting
  the first member, who registers, posts, is approved, is quoted in a reply
  and reacts (`setup.spec.ts`);
- logging in (with a password, an emailed link and an emailed code),
  inviting someone and joining with the invite, downloading your data and
  deleting your account, uploading an image with the plain form, and the
  search, terms and privacy pages (`accounts.spec.ts`, `pages.spec.ts`);
- writing: threads, replies, quotes, quote-forks, edits, reactions and
  deleting (`writing.spec.ts`);
- private categories and groups (`privacy.spec.ts`);
- direct messages and their notifications (`messages.spec.ts`);
- reporting and moderating, and finding an image to remove
  (`moderation.spec.ts`);
- the JavaScript islands: reacting without a reload, the composer's preview,
  images and selection quoting, the live "N new posts" bar and the
  notification badge and menu (`islands.spec.ts`).

Each spec runs in four projects without JavaScript (320px and 1280px, light
and dark) and two with it (`js-1280` and `js-320`), which see the islands
where the others see the plain forms they replace, and check that the
islands' menus, dialogs and bars fit the screen. `islands.spec.ts` runs only
with JavaScript; `setup.spec.ts` runs in two projects of its own, each
against an empty forum. Each project logs in as seeded members of its own
where it can, since logging in is rate limited per account.

Email goes over SMTP to the dev compose Mailpit, which the tests read
through its API (`E2E_MAILPIT_URL`, default http://127.0.0.1:8025).

    docker compose -f ../dev/compose.yaml up -d    # Postgres and Mailpit
    (cd ../web && npm install)
    npm install
    npx playwright install chromium
    npm test

`server.mjs` builds the islands (`npm run build` in `../web`) and the
server, recreates a `parlour_e2e` database on the dev Postgres, fills it
with `parlour seed` and serves it on port 18080 (`E2E_PORT`). It also
serves two empty forums on the next two ports for the setup tests, writing
the setup links they print to `.setup-links/`. Set `E2E_POSTGRES_URL` to
use another Postgres server. The server binary is built with
`SQLX_OFFLINE=true`, so `server/.sqlx` must be up to date.
