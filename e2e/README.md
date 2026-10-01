# Browser tests

Playwright tests for Parlour's pages (DESIGN.md §12). They check that every
reading page renders without JavaScript, fits a 320px screen with no
horizontal scroll, works in light and dark mode, and makes no requests to
other hosts.

    docker compose -f ../dev/compose.yaml up -d
    npm install
    npx playwright install chromium
    npm test

`server.mjs` recreates a `parlour_e2e` database on the dev Postgres, fills it
with `parlour seed` and serves it on port 18080 (`E2E_PORT`); set
`E2E_POSTGRES_URL` to use another Postgres server.
