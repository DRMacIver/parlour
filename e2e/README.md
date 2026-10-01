# Browser tests

Playwright tests for Parlour's pages and main flows (DESIGN.md §12). They
check that every page renders without JavaScript, fits a 320px screen with
no horizontal scroll, works in light and dark mode, and makes no requests to
other hosts; and they walk through logging in (with a password, an emailed
link and an emailed code), inviting someone and joining with the invite.
Email goes over SMTP to the dev compose Mailpit, which the tests read
through its API (`E2E_MAILPIT_URL`, default http://127.0.0.1:8025).

    docker compose -f ../dev/compose.yaml up -d    # Postgres and Mailpit
    npm install
    npx playwright install chromium
    npm test

`server.mjs` recreates a `parlour_e2e` database on the dev Postgres, fills it
with `parlour seed` and serves it on port 18080 (`E2E_PORT`); set
`E2E_POSTGRES_URL` to use another Postgres server.
