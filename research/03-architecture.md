# 03 — Technical architecture research

Research date: 2026-10-01. Crate and package versions below came from the crates.io / npm / GitHub APIs on that date.

Goal: a minimal, self-hosted forum. Rust backend, React frontend. It must support public and private categories and threads. The anonymous and crawler view must be very cheap to serve, logged-in users must get a fast, pleasant UI, and the whole thing should run on one small server.

**Project constraints this document is weighted for:**

- **Threads** are flat and chronological, with no nesting. A post can be **quoted into a new top-level thread**, which may sit in a different category with different visibility.
- **Visibility is per-category only.** "Private" means members-only categories, optionally narrowed to a subset of members via groups. There are no per-thread ACLs.
- **User-to-user DMs/private mail** must never reach caches, search results, or feeds that other users can see.
- **Scale** is about 500 users, possibly with a second community.
- **Hosting** is a single small VPS with an optional CDN. Anything bigger is out of scope.
- **Distribution:** open-source, general-purpose software that others deploy, probably with docker compose. Deployment ergonomics matter a lot: one image, sensible defaults, minimal config, automatic migrations, simple backups, TLS via a Caddy sidecar, SMTP config, and a first-run admin setup. Discord login is one optional provider among several.

**Decision taken by the user: PostgreSQL**, running as a service in the same docker compose stack on the same VPS ("just use postgres in the absence of a compelling story to the contrary"). This document therefore covers Postgres specifics and does not revisit SQLite. At this scale (about 500 users, peak writes of a few per second, a database in the hundreds of MB for years), a single Postgres 18 with a few hundred MB of RAM is far more than enough.

---

## TL;DR (opinionated)

| Area | Recommendation | Main alternative |
|---|---|---|
| Rendering | **Server-rendered HTML for every page view**, from Rust templates (askama). Logged-in users also get a React bundle that mounts **islands** (composer, notifications, live updates, reactions, mod tools). Post bodies are "cooked" to HTML once, at write time. | Discourse-style split: Rust-template HTML for anon and crawlers, a full React SPA for logged-in users. |
| JS-in-Rust SSR | **Don't.** Embedding V8 (ssr_rs/deno_core) or QuickJS adds a lot of complexity for little gain once post HTML is pre-cooked. | ssr_rs, if you later want a single React codebase for all markup. |
| Anon caching | An in-process full-page cache (moka) used only for requests with no session cookie, plus generation-based ETags and `Cache-Control: public, s-maxage, stale-while-revalidate`. A CDN or Caddy is optional and goes on top. | nginx/Caddy microcache, Cloudflare Cache Rules. |
| Leak prevention | **Separate code paths, not `Vary: Cookie`.** Anon entries are only ever produced by rendering as the Anonymous principal. Anything personalised gets `private, no-store`. | — |
| Permissions | **Category-level see/reply/create permissions granted to groups** (`everyone`, `members`, custom groups). DMs are a completely separate subsystem with their own tables. All data access goes through a `Viewer`-typed repository layer. Quotes copy content only if the source is at least as visible as the destination. | — |
| Multiple communities | **One instance per community** (separate container and `/data` volume), with Caddy routing by hostname | Multi-tenant single process (not worth it) |
| First-run setup | One-time setup token printed to the logs leads to a `/setup` wizard that creates the admin and sets name, URL, SMTP test and login providers | `forum create-admin` CLI |
| Web framework | **axum 0.8.9** + tower-http 0.7.1 + tokio 1.53 | actix-web 4.15 |
| DB | **PostgreSQL 18** (official `postgres:18` image) as a compose service, tuned for a small VPS | — (user decision) |
| DB access | **sqlx 0.9** (compile-time checked SQL against a committed `.sqlx` offline cache; `sqlx::migrate!` embedded and run at startup) | diesel 2.3 + diesel-async |
| Search | **Postgres FTS**: a stored generated `tsvector` column with a GIN index, `websearch_to_tsquery`, `ts_rank_cd`, `ts_headline`, joined to the permission filter in the same query | tantivy 0.26 only if relevance, typo tolerance or CJK become real problems |
| Live updates / invalidation | **LISTEN/NOTIFY** via `sqlx::postgres::PgListener`: one listener connection fans out to in-process `broadcast` channels and the page cache | In-process only (fine while there is a single app process) |
| Backups | **Nightly `pg_dump -Fc` sidecar** with rotation (default, zero-config) plus an optional off-site copy; **pgBackRest** (2.59) WAL archiving to S3 as an opt-in "advanced" profile | wal-g 3.0 |
| Markdown | **comrak 0.55** (GFM) or pulldown-cmark 0.13, then **ammonia 4.2** to sanitise | — |
| Auth | **Opaque server-side sessions** in the DB (not JWT). Email magic links + passkeys (webauthn-rs 0.5.5), optional password (argon2 0.6). Optional OAuth providers (**Discord**, GitHub, Google, generic OIDC) are configured through the admin UI, and none is assumed. | — |
| Email | lettre 0.11.23 over SMTP | HTTP APIs (Postmark etc.) as an optional extra |
| Jobs | A Postgres `jobs` table, claimed with `FOR UPDATE SKIP LOCKED` and woken by NOTIFY (about 200 lines, transactional enqueue) | graphile_worker 0.13.6 (Rust port), apalis-postgres (1.0 RC), underway 0.2 |
| Frontend | Vite 8 + React 19.3 for logged-in islands only. Anon pages ship about 0–5 KB of JS. | Preact 11 (`preact/compat`) for size; htmx 2/4 |
| Real-time | **SSE** (axum built-in) fed by a `tokio::sync::broadcast` per topic | WebSockets (not needed) |
| Deploy | One static musl binary with embedded, pre-compressed assets (memory-serve / rust-embed) and embedded migrations, shipped as a tiny Docker image. The reference `compose.yaml` runs **forum + postgres + caddy + backup**. Required config: domain, SMTP, and a generated DB password. | Bare binary + systemd + distro Postgres |

---

## 1. Rendering strategy

### 1.1 How existing fast forums do it

- **Discourse** is an Ember SPA. For crawlers it **sniffs the user agent and serves a separate, JS-free "crawler" layout** (`app/views/layouts/crawler.html.erb`), rendered by Rails from server templates. Old browsers get the same view via `browser_update_user_agents`. Normal browsers get the app, with `<noscript>` content as a fallback. The cost is two parallel renderers, and the crawler view has historically been noticeably worse than the app: thin markup, and pagination and SEO quirks that people keep raising on meta. Sources: [crawler.html.erb](https://github.com/discourse/discourse/blob/main/app/views/layouts/crawler.html.erb), [What Googlebot sees when crawling Discourse](https://meta.discourse.org/t/what-googlebot-sees-when-crawling-discourse/212726), [noscript view get-option discussion](https://meta.discourse.org/t/add-a-get-option-to-enable-noscript-view-of-a-page/33823), [Evil Trout: Adding support for search engines to JS apps](https://fishtank.eviltrout.com/t/adding-support-for-search-engines-to-your-javascript-applications/22), [paid request to let crawlers see the JS version](https://meta.discourse.org/t/paid-work-allow-search-engines-to-crawl-the-production-js-version/222933).
- **Flarum** works the same way. It is a Mithril SPA, and "the few templates that are present in the backend are only used to render search-engine-optimized content". It has had crawler pagination bugs as a result ([Flarum frontend docs](https://docs.flarum.org/extend/frontend/), [flarum#1310](https://github.com/flarum/framework/issues/1310)).
- **Hacker News** serves plain server-rendered HTML from one process on one machine. Logged-out traffic (90–99%) gets cached pages, and it has run with and without Cloudflare ([HN thread](https://news.ycombinator.com/item?id=23875692), [surviving the HN front page](https://danielpetrica.com/surviving-hacker-news-front-page/)).
- **Lobsters** is plain Rails server-rendered HTML with a cache for the logged-out view. A recent bug shows the main hazard: **deleted stories kept being served from the logged-out cache** ([lobsters#2209](https://github.com/lobsters/lobsters/issues/2209)). Explicit invalidation on moderation actions is mandatory.
- **old.reddit** is server-rendered HTML with a small amount of jQuery-era JS. Reported comparisons put its JS at roughly 180 KB against new Reddit's roughly 890 KB, with far fewer requests ([redcurate comparison](https://redcurate.com/blog/old-reddit), [cole-k on old reddit 2026](https://www.cole-k.com/2026/07/21/reddit/)).

**Lesson:** the fastest forums are plain HTML, and the SPA forums all bolted a second, worse HTML renderer on for crawlers. If we are going to write an HTML renderer anyway, make it **the** renderer.

### 1.2 Options compared

| Option | Anon perf | Logged-in UX | SEO / a11y / no-JS | Complexity |
|---|---|---|---|---|
| A. Pure server HTML (HN/Lobsters) | Best | OK, with page reloads and a bit of JS | Best | Lowest |
| B. React SPA only | Bad: JS required, slow first paint | Good | Poor without SSR | Medium |
| C. React SSR + hydration (Next-style) | Medium: hydration JS on every page | Good | Good | High. A Rust server would need an embedded JS runtime. |
| D. Discourse split: HTML for anon + SPA for logged-in | Best for anon | Good | Good for anon only | High: two renderers drift apart |
| **E. HTML everywhere + React islands for logged-in** | **Best** | **Good**: islands where interactivity matters, live updates via SSE | **Best**: logged-in users get the same accessible HTML | **Medium-low** |

**Recommendation: E.** Every URL returns a complete HTML page rendered in Rust. If a session is present, the page also includes `<script type=module src=/assets/app-[hash].js>` and some `data-island="composer"` mount points with JSON props embedded in `<script type="application/json">`. React then mounts:

- the reply/new-topic composer (markdown editor, preview, drafts, uploads, @mentions);
- the notifications bell/menu and the unread counters;
- the "N new posts, click to load" live update bar, fed by SSE;
- the likes/reactions buttons, the bookmark and watch toggles, and the mod menus;
- optionally, client-side navigation later, if wanted. An islands app can grow into option D without a rewrite, because the JSON endpoints already exist.

Why not hydrate whole pages with React? Hydration has to re-render the same tree on the client, so you pay for the React runtime plus the full page data twice. Forum post bodies are user-authored HTML that React should not own anyway: insert them as pre-sanitised HTML via `dangerouslySetInnerHTML` or simply leave them as server DOM.

**Key trick: "cooked" HTML.** Render markdown to sanitised HTML **once, at write time**, and store `raw` and `cooked` side by side, as Discourse does. Store a `cook_version` so a background "rebake" can re-render after renderer upgrades. The page templates then only concatenate trusted HTML fragments, so rendering a topic page is mostly string building. That is microseconds in askama.

### 1.3 SSR of React from Rust: viable but not recommended

- **ssr_rs** embeds V8 (rusty_v8) and calls a `renderToString` function from a Vite/webpack bundle. It is the backbone of the Tuono framework ([crates.io/ssr_rs](https://crates.io/crates/ssr_rs), [repo](https://github.com/Valerioageno/ssr-rs), [guide](https://medium.com/@valerioageno95/step-by-step-guide-to-server-side-render-react-with-rust-640519ec48dc)). The costs: tens of MB of V8 in the binary, a slow cross-compile/musl story, per-thread isolates, and a JS build step that becomes a hard dependency of the server binary.
- **deno_core 0.412** and **rquickjs 0.14** are lower-level alternatives. QuickJS is small but much slower than V8 for React rendering.
- Either way you need a Node/Vite build for the server bundle, and you lose most of what makes Rust nice here: a single static binary and simple deploys.

**Verdict:** use Rust templates for markup and share **CSS** (and class names) between the templates and the React islands. Islands only render widget-internal markup, so there is very little duplicated markup. The one real duplication is "render a single post": the server needs it, and the client needs it to insert live posts. Solve that by having the server return **rendered HTML fragments** for new posts (`GET /t/:id/posts?after=N&format=fragment`). The client inserts them, so it never re-implements post markup. This is the htmx idea, used from React.

### 1.4 Template engine choice

| Crate | Version | Notes |
|---|---|---|
| **askama** | 0.16.1 (2026-09) | Jinja-like syntax compiled to Rust at build time. Type-checked and very fast. `askama_web` 0.16 gives axum integration. **Default pick.** |
| maud | 0.27.0 (2025-02) | HTML as a Rust macro DSL. Lovely and type-safe, but releases are slower and designers can't edit it. |
| minijinja | 2.24.0 (2026-09) | Runtime Jinja2. Pick it if you want **admin-overridable themes/templates** without recompiling. Errors show up at runtime instead of compile time. |
| hypertext | 0.12.1 | maud/rsx-like with lazy rendering. Smaller community. |

Pick askama for core pages. minijinja is an option if user-customisable templates become a goal later.

### 1.5 SEO, accessibility, no-JS

- Server HTML gives correct `<title>`, `<meta name=description>`, canonical URLs, OpenGraph tags, `<link rel=next/prev>`, real `<a href>` pagination, and JSON-LD `DiscussionForumPosting` for free, with no UA sniffing needed. Serving everyone the same HTML also avoids any appearance of cloaking.
- Make the core read path, plus login, posting a reply, and search, work with **plain `<form>` POSTs**. Islands progressively enhance them. Screen readers get semantic HTML (`<article>` per post, headings, landmarks), and keyboard navigation works without JS.
- Provide `sitemap.xml` generated from **public** topics only (see §3).

---

## 2. Caching the anonymous view

### 2.1 Principle: anon is one audience

Every anonymous visitor and every crawler sees **byte-identical HTML** for a given URL. Make that true by construction:

- Anon HTML contains **no** per-user data: no CSRF token, no "hello, X", no flash messages, no A/B buckets.
- Login and sign-up forms on anon pages either POST to an endpoint that uses a double-submit or `SameSite` + `Origin` check (no token embedded in the page), or link to a separate `/login` page that is never cached.
- **Never set cookies on anon responses.** Shared caches won't store responses with `Set-Cookie` (Cloudflare refuses by default), and it would leak session identifiers ([stackharbor: cookies through Cloudflare](https://stackharbor.com/en/knowledge-base/cfops-cookies-through-cloudflare/)).

### 2.2 Layered design

1. **In-process page cache (main layer).** A `moka` 0.12 async cache with a byte-weigher, sized for example at 64–256 MB. The key is `(route, normalised query params, representation)`. "Representation" must include **every request attribute that changes the body**: HTML vs JSON vs RSS, `Accept-Encoding` bucket, and `X-Requested-With`/XHR. Discourse's **CVE-2024-47773** came from exactly this: XHR and normal requests shared a cache key, so attackers poisoned the anon cache with a response missing preloaded data ([GHSA-58vv-9j8h-hw2v](https://github.com/discourse/discourse/security/advisories/GHSA-58vv-9j8h-hw2v)). Store **pre-compressed** (brotli + gzip) bodies, so a cache hit is a memcpy. Discourse caches anon pages in Redis for about 60 s with a "store only after N hits" threshold, so crawlers don't fill it with one-off pages ([discourse_defaults.conf](https://github.com/discourse/discourse/blob/main/config/discourse_defaults.conf), [anon cache perf review](https://review.discourse.org/t/feature-improve-performance-of-anonymous-cache/5518)). We can use moka's TinyLFU admission policy for the same effect.

2. **Invalidation by generation counters, not TTL guessing.** Keep in-memory (and DB-persisted) monotonically increasing versions:
   - `site_gen`: bumped by settings, theme, or category/permission changes. It invalidates everything.
   - `category_gen[c]`: bumped by a new topic, a topic move, or a delete in category `c`.
   - `topic_gen[t]`: bumped by a new post, edit, delete, hide, or moderation action in topic `t`.

   The generations are bumped **in the same Postgres transaction** as the write: an `UPDATE cache_generations …` followed by `SELECT pg_notify('forum_gen', 'topic:123')`. NOTIFY is transactional, so it is delivered only after commit and never for a rolled-back write. A single `PgListener` task in the app receives it and updates the in-memory generation map, which invalidates the page cache and drives SSE (§5). With one app process you could skip NOTIFY and bump in-process after commit, but going through Postgres is barely more code and keeps a second process (or a separate worker container) correct for free.

   A cache entry records the generations it depended on. A lookup is valid only if they still match, which costs a few integer compares. This makes invalidation **exact and immediate**. Fixed TTLs are what produced the Lobsters "deleted story still visible" bug. The same generations produce the **ETag** (`W/"t123-g456-s7"`), so conditional `If-None-Match` requests get `304` without rendering, and often without touching the DB.

3. **HTTP headers for anon responses:**
   ```
   Cache-Control: public, max-age=0, s-maxage=30, stale-while-revalidate=60, stale-if-error=86400
   ETag: W/"..."
   ```
   - Browsers always revalidate (cheap 304s), so users see new posts immediately.
   - Shared caches (Caddy, nginx, CDN) may serve for 30 s and serve stale content while revalidating. `stale-if-error` keeps the site readable during restarts.
   - See [MDN Cache-Control](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Cache-Control) and [Cache-Control for Civilians](https://csswizardry.com/2019/03/cache-control-for-civilians/).

4. **Logged-in and private responses:** `Cache-Control: private, no-store`. MDN explicitly warns that forgetting `private` on personalised responses lets shared caches serve them to other users.

5. **Static assets:** hashed filenames with `Cache-Control: public, max-age=31536000, immutable`.

6. **Optional reverse proxy or CDN** (for the admin who wants it):
   - **Caddy** 2.11.4: automatic HTTPS. The `cache-handler` module (Souin) supports RFC-compliant caching and cookie-based bypass matchers ([caddyserver/cache-handler](https://github.com/caddyserver/cache-handler), [Caddy WordPress cookie example](https://caddy.community/t/some-questions-regarding-the-cache-handler-souin-module-and-wordpress/26541)).
   - **nginx** microcaching with `proxy_cache_bypass $cookie_session; proxy_no_cache $cookie_session;` ([nginx content caching](https://docs.nginx.com/nginx/admin-guide/content-cache/content-caching/), [microcaching guide](https://www.getpagespeed.com/server-setup/nginx/nginx-proxy-cache-microcaching)).
   - **Cloudflare** never caches HTML by default. On any plan you can add a Cache Rule "Eligible for cache when Cookie does not contain `__Host-fsid`". "Bypass cache on cookie" in Page Rules is Business+ only ([stackharbor: cookie bypass](https://stackharbor.com/en/knowledge-base/cfops-cookies-cache-bypass/), [Cloudflare Cache Rules settings](https://developers.cloudflare.com/cache/how-to/cache-rules/settings/), [Origin Cache Control](https://developers.cloudflare.com/cache/concepts/cache-control/)).
   - The in-process cache must be good enough that **none of these are required**. With 30 s `s-maxage`, a CDN can serve stale content after a delete for up to about 30 s. For moderation-sensitive deletes, offer an optional purge hook (Cloudflare API / Souin PURGE).

### 2.3 Making sure private content never leaks into caches

**Prefer structural guarantees over `Vary: Cookie`.**

- `Vary: Cookie` is nearly useless in practice. Any analytics or consent cookie fragments the cache, and Cloudflare largely ignores `Vary` for HTML. Use it only as belt-and-braces.
- **Two code paths:** the request extractor yields either `Viewer::Anonymous` or `Viewer::User(..)`. Only the `Anonymous` branch consults or populates the page cache. **A cache entry is only ever produced by rendering as `Anonymous`.** Never render as a user and then strip.
- A request **with** a session cookie bypasses the anon cache entirely, even if the session is invalid. In that case clear the cookie and redirect, so the next request is cacheable.
- Private resources return `404` (not `403`) to viewers without permission, with `private, no-store`. Never cache negative or private responses in the shared tier.
- Name the session cookie with the `__Host-` prefix (`Secure`, `Path=/`, no `Domain`). The proxy and CDN rules then match on one well-known name.
- **Test it:** a property-based integration test that creates random categories, permissions and posts with unique marker strings, then crawls every anon route (HTML, JSON, RSS, sitemap, search, user pages) and asserts that no private marker ever appears. It should also assert that every anon response lacks `Set-Cookie` and that every authenticated response has `private`.

---

## 3. Permission model

### 3.1 Recommended model (matches the project's decisions)

Visibility is **per category only**. Threads are flat and chronological. DMs are a separate subsystem.

- **Groups** are the only thing permissions are granted to. Built-in pseudo-groups are `everyone` (including anon and crawlers), `members` (any logged-in, approved account), `moderators` and `admins`. Admins create custom groups (e.g. "staff", "organisers", "paid members"), and users belong to many groups.
- **Category permissions:** a row `(category_id, group_id, level)` where level ∈ {`see`, `reply`, `create`}. This gives the three tiers the project wants:
  - **Public:** `everyone: see`, `members: create`.
  - **Members-only:** `members: see/create`, no `everyone` row.
  - **Group-gated:** only `group X: see/create`.

  This is Discourse's category "security" model minus its complexity. There is no implicit inheritance between parent and child categories. Store an explicit permission set on each category, and have the UI warn when a child is more visible than its parent.
- **Denormalise the visibility tier.** Store `categories.visibility ∈ {public, members, restricted}` and copy it onto `topics.visibility` in the same transaction as any move or permission change. The hot anon path becomes `WHERE t.visibility = 'public'`, which is trivial, indexable and cacheable. All non-public checks go through the group join.
- **No per-thread ACLs.** Every thread inherits its category's visibility, and moving the thread is the only way to change it. This removes a large class of bugs. Discourse needed a separate plugin for per-topic privacy ([Private Topics plugin](https://meta.discourse.org/t/private-topics-plugin/268646)).
- **DMs / private mail are a separate subsystem.** They get their own tables (`dm_conversations`, `dm_participants`, `dm_messages`), not "topics in a hidden category", so no topic listing, search, feed, sitemap, profile-activity or cache code path can ever select them by accident. The rules:
  - DM pages are always `Cache-Control: private, no-store` and never touch the page cache.
  - DMs have **their own search** (participant-scoped, `WHERE participant = viewer`), or none in v1. They are never in the main FTS query.
  - DMs never appear in RSS, sitemaps, digests, or anyone else's activity stream.
  - Email notifications for a DM go to the participants only and include the content only if the user opted in.
  - Admin access to DMs (for abuse handling) is an explicit, audited action, not a side effect of being admin.
  - Discourse's group-PM title leak shows how easy it is to leak participant lists and titles ([GHSA-vm3x-w6jm-j9vv](https://github.com/discourse/discourse/security/advisories/GHSA-vm3x-w6jm-j9vv)), and NodeBB leaked private chats through an endpoint that skipped signature checks on GET ([Aikido](https://www.aikido.dev/blog/eight-high-severity-vulnerabilities-nodebb)).
- **Quote-forking a post into a new top-level thread** is the project's biggest built-in cross-visibility risk. A post in a group-gated category gets quoted into a new thread in a public category. Rules:
  1. Define a visibility ordering: `public` > `members` > `restricted(group set)`. A quote of post P into destination category D may **embed P's content only if every viewer of D can already see P**, i.e. `audience(D) ⊆ audience(P)`. For group-gated sources, accept only the trivial cases (same category, or a destination whose groups are a subset) rather than computing general set containment.
  2. If that check fails, the composer **refuses or warns**. The default is to refuse with a "quoting from a more private category into a less private one is not allowed" message. A moderator-only override is available, and it records an audit entry naming who declassified what.
  3. **Store quotes as structured references** (`quoted_post_id`) and render them at view time with the viewer's permissions. Don't just paste text into the raw markdown. This also handles the source later becoming more private: the quote collapses to "[quoted post not available]" for viewers who can't see it. If quoting copies text into `raw` (as Discourse's `[quote]` BBCode does), the check in (1) must also run on **edit**, and on **move of either thread**.
  4. The "forked from" backlink shown in the source thread is fine, because it is visible only to source viewers. The link from the new thread back to the source must be hidden from viewers who can't see the source, or it leaks the title.
  5. These are exactly the bug classes behind the Discourse onebox bypass ([HackerOne 312647](https://hackerone.com/reports/312647)) and the Flarum mentions leak of every post ([CVE-2023-22487](https://github.com/flarum/framework/security/advisories/GHSA-22m9-m3ww-53h3)).
- **Site-wide "login required" mode** is just "no category grants `everyone`", plus the anon cache disabled. Test it explicitly: Discourse shipped a regression that exposed homepage content on login-required sites ([GHSA-v3h7-c287-pfg9](https://github.com/discourse/discourse/security/advisories/GHSA-v3h7-c287-pfg9)).
- **A second community** is a separate instance (§6), not a tenant inside the same database, so no cross-community leaks are possible.

### 3.2 Enforcing it consistently (the hard part)

Nearly every forum leak in the record is **an endpoint or side-channel that forgot to check permissions**, not a flaw in the model. Design so that forgetting is a compile error.

- **One gate in the type system.** All post/topic reads go through a repository taking `&Viewer`. The viewer carries a precomputed `readable_category_ids: Arc<[CategoryId]>`, cached per user and invalidated by `site_gen` or the user's group generation. DM access is checked separately against `dm_participants`. Return types like `Visible<Topic>` can only be constructed inside that module. Raw `sqlx::query!` for content tables is lint-banned outside it.
- Every listing query includes `category_id IN (readable)` or `visibility = 'public'`. In Postgres, bind the set as an array: `WHERE t.category_id = ANY($1::bigint[])`, which sqlx supports directly with `&[i64]`. *Optional defence in depth:* Postgres row-level security keyed on `current_setting('forum.viewer_groups')`. It is probably not worth the complexity and the pooled-connection footgun, but it is a possible later hardening.
- **Re-check at time of use, not time of creation.** Leak channels to cover:

| Channel | Pitfall | Example in the wild | Rule |
|---|---|---|---|
| Search | Index includes private posts; filter applied only in UI | — | The tsvector lives on `posts`; the search query joins `topics` and applies the viewer filter in the same SQL; anon search = `visibility='public'` only; DMs are never in this query |
| RSS/Atom feeds | Feed readers can't auth; feeds built from "latest" ignoring perms | — | Feeds rendered as **Anonymous** only; optional per-user secret-token feeds with full perms and revocation |
| Notifications | Notification created while allowed, viewed after the topic moved or the user was removed | Discourse bookmark reminders after the security change, [CVE-2023-45816](https://vulners.com/search/vendors/discourse/products/discourse) | Store references, not copied text; re-check on render, drop invisible ones |
| Emails/digests | Digest built with the wrong principal | Discourse [GHSA-q9jp-xv4g-328f](https://github.com/discourse/discourse/security/advisories/GHSA-q9jp-xv4g-328f) leaked an admin's digest | Build each email **as the recipient** and re-check at send time (jobs may be delayed) |
| Quotes, quote-forks, mentions, link previews ("oneboxes") | Quoting or linking a private post into a public one copies its content (see the quote-fork rules in §3.1) | Discourse onebox bypass ([HackerOne 312647](https://hackerone.com/reports/312647)); Flarum mentions leaked **every post** via the JSON relationship ([CVE-2023-22487](https://github.com/flarum/framework/security/advisories/GHSA-22m9-m3ww-53h3)) | At cook time, expand quotes or previews **only if the source is visible to `everyone`** (or to the target category's audience); otherwise render a bare link. Never include related posts' payloads in API responses without per-item checks |
| User profiles / activity streams | "Recent posts by X" ignores category perms | Discourse user-action endpoint leaked private titles and excerpts ([GHSA-824f-66wh-xx3g](https://github.com/discourse/discourse/security/advisories/GHSA-824f-66wh-xx3g), 2026) | Same repository gate |
| Reactions / likes / read state | Secondary tables queried directly | discourse-reactions ([GHSA-9358-hwg5-jrmh](https://github.com/discourse/discourse-reactions/security/advisories/GHSA-9358-hwg5-jrmh)); read state leak ([commit](https://github.com/discourse/discourse/commit/aed65ec16d38886d7be7209d8c02df4ffd4937a4)) | Join through topics with the viewer filter |
| Category metadata | Existence of secret categories via counts, backgrounds, tags | [CVE-2024-28242](https://vulners.com/search/vendors/discourse/products/discourse); hidden-tag exclusion leak (CVE-2023-23624) | Hidden categories are invisible everywhere: counts, tag lists, "latest" timestamps, sitemap |
| Edit history / revisions | Revisions served without checks; content moved private→public reveals old private text | — | Revisions inherit topic visibility; on move to a more public category, warn about history; let mods purge revisions |
| Uploads / attachments | Files in private posts served from a public static path | — | Uploads referenced only from private content are served by an auth-checking handler with `private` caching; public uploads from immutable static URLs |
| Federation / webhooks / APIs | New endpoint forgets checks | NodeBB private category content via an ActivityPub endpoint ([The Hacker News, 2026](https://thehackernews.com/2026/07/nodebb-patches-eight-ai-found-flaws.html), [Aikido write-up](https://www.aikido.dev/blog/eight-high-severity-vulnerabilities-nodebb)) | Same gate; out of scope for v1 |
| SSE / live updates | Subscriber keeps receiving after losing access | — | Check on subscribe and on each event delivery; drop streams on group changes; NOTIFY payloads carry ids only, never content |
| DMs | DM content reaching shared caches, search, feeds, digests, activity streams | Discourse group-PM leak; NodeBB chat leak | Separate tables and code paths; `private, no-store`; participant-scoped search only (§3.1) |
| Caches | Private content rendered into the anon cache | Discourse anon cache poisoning (above) | §2.3 |
| Moving topics | Public → private leaves copies in caches, CDN, search, feeds | Lobsters deleted-story cache | Moves bump `site_gen`/`category_gen` for both categories, reindex, and purge the CDN if configured |

---

## 4. Rust stack

Latest versions as of 2026-10-01, from crates.io:

| Purpose | Crate | Version | Notes |
|---|---|---|---|
| Async runtime | tokio | 1.53.1 | |
| Web framework | **axum** | 0.8.9 (2026-04) | Tower ecosystem, built-in SSE and WebSocket support, the most widely used choice. actix-web 4.15.0 is equally fast, but its ecosystem is separate from tower. **Choose axum.** |
| Middleware | tower-http | 0.7.1 | compression, `ServeDir`, tracing, timeouts, `SetResponseHeader` |
| Templates | askama / askama_web | 0.16.1 / 0.16.0 | see §1.4 |
| SQL | **sqlx** (postgres feature) | 0.9.0 (2026-05) | compile-time-checked queries, `migrate!` embeds migrations. 0.9 adds `sqlx.toml` and the `SqlSafeStr` injection guard ([0.9 announcement](https://github.com/launchbadge/sqlx/discussions/4271), [CHANGELOG](https://github.com/launchbadge/sqlx/blob/main/CHANGELOG.md)) |
| Alt SQL | tokio-postgres 0.7.18 + deadpool-postgres 0.14.2 | | lower-level and no compile-time checks; not needed |
| ORMs | diesel 2.3.13, sea-orm 2.0.4 | | Not worth it for a forum's mostly hand-tuned queries |
| Cache | moka | 0.12.16 | anon page cache, viewer-permission cache |
| Search | Postgres FTS (built-in); tantivy 0.26.2 if ever needed | | |
| Markdown | comrak 0.55.0 / pulldown-cmark 0.13.4 | | |
| Sanitise | ammonia | 4.2.0 | |
| Sessions | tower-sessions 0.15.0 (+ axum-login 0.18) | | or hand-roll (≈150 lines) |
| Passwords | argon2 | 0.6.0 | |
| Passkeys | webauthn-rs | 0.5.5 | |
| OAuth/OIDC | oauth2 5.0.0, openidconnect 4.0.1 | | |
| TOTP (optional 2FA) | totp-rs | 6.0.0 | |
| Email | lettre | 0.11.23 | |
| Jobs | hand-rolled SKIP LOCKED queue; alternatives graphile_worker 0.13.6, apalis-postgres 1.0.0-rc.9, underway 0.2.0, sqlxmq 0.6.0 | | §4.7 |
| Rate limiting | governor / tower_governor | 0.10.4 / 0.8.0 | |
| Images | image 0.25.10 + fast_image_resize 6.1.0 | | |
| Asset embedding | memory-serve 2.4.0 / rust-embed 8.12.0 | | |
| Rust→TS types | ts-rs | 12.0.1 | share API types with React |
| Logging | tracing | 0.1.44 | |
| Database server | PostgreSQL | 18.6 (Docker `postgres:18`) | 19 is in beta (19beta4); pin the major version |

### 4.1 PostgreSQL: deployment and tuning for a small VPS

**Decision: Postgres 18** (latest stable 18.6 as of 2026-10-01; 19 is in beta). It runs as the official `postgres:18` image in the compose stack. **Pin the major version** in the compose file (`postgres:18`, not `latest`), because a major upgrade needs a data migration (see below).

**Tuned config for a small VPS (1–2 GB RAM shared with the app and Caddy).** Ship this as a `postgres.conf` mounted into the container, or as `command: postgres -c …` flags in compose:

```
max_connections = 30            # app pool ~10 + listener + backup + admin; no PgBouncer needed
shared_buffers = 256MB          # ~20-25% of RAM (use 512MB on a 2-4 GB box)
effective_cache_size = 768MB    # ~50-75% of RAM; planner hint only
work_mem = 8MB                  # per sort/hash node; small pool keeps this safe
maintenance_work_mem = 64MB     # vacuum / index builds
wal_buffers = 8MB
checkpoint_completion_target = 0.9
max_wal_size = 1GB
min_wal_size = 80MB
random_page_cost = 1.1          # SSD/NVMe VPS
effective_io_concurrency = 200
huge_pages = off                # small containers
jit = off                       # JIT hurts short OLTP queries
log_min_duration_statement = 250ms   # surfaces slow queries in `docker logs`
idle_in_transaction_session_timeout = 60s
```

These follow the common pgtune rules: shared_buffers at 20–25% of RAM, maintenance_work_mem at about 10%, and max_connections kept small with an app-side pool ([pgtune](https://pgtune.leopard.in.ua/), [EDB memory tuning](https://www.enterprisedb.com/postgres-tutorials/how-tune-postgresql-memory), [Bun/Uptrace tuning guide](https://bun.uptrace.dev/postgres/performance-tuning.html), [USAVPS VPS tuning](https://usavps.com/blog/post/postgresql-production-tuning-vps/)). The app's sqlx `PgPool` gets `max_connections ≈ 10`. Postgres in this setup idles at roughly 50–100 MB RSS plus shared_buffers.

**Write hot spots:** read-position tracking ("last read post" per user per topic) and view counters. Batch them in memory and flush every few seconds with a single `INSERT … ON CONFLICT DO UPDATE` using `unnest()` arrays, so they don't churn WAL or bloat tables. Autovacuum defaults are fine at this size.

**Networking/security in compose:** Postgres sits on an internal-only network with no `ports:` published. The password comes from a generated `.env` / docker secret (`POSTGRES_PASSWORD_FILE`).

**Major-version upgrades in Docker (a real footgun for non-experts):**

- Data directories are not forward-compatible across majors, so upgrading needs `pg_dump`/restore or `pg_upgrade` with both binaries present.
- **The Postgres 18 image changed `PGDATA`** to `/var/lib/postgresql/18/docker`, and the `VOLUME` to `/var/lib/postgresql`. If you bump an old compose file's tag without moving the mount, the entrypoint **silently initialises a new, empty cluster** instead of failing ([Postgres 18 Docker silently ignores your named volume](https://rdiachenko.com/posts/databases/postgresql/postgres-18-docker-silently-ignores-your-named-volume/), [official image docs](https://hub.docker.com/_/postgres), [17→18 migration walkthrough](https://www.nite07.com/en/posts/postgres-docker-18-migration/), [Immich upgrade gist](https://gist.github.com/LukaPrebil/322831c0aaed075ebffbb40a3ba1889f)).
- **Since we start on 18, mount the volume at `/var/lib/postgresql`** (the new convention). Future upgrades are then version-scoped directories under one volume.
- **Make the app defend against this:** on startup, if the DB has zero forum tables but `/data` contains uploads or a previous `instance_id` marker, refuse to run the "fresh install" path and print a loud error. The app should also record `instance_id` in the DB and a file in its volume, and check that they match.
- Ship a documented `forum-upgrade-postgres` procedure. It amounts to: `docker compose exec forum forum backup`, change the tag, bring up the new empty cluster, `forum restore`. A helper image such as `pgautoupgrade/pgautoupgrade` is an option, but plain dump/restore is the most understandable path for a small DB (seconds to minutes).

### 4.2 sqlx with Postgres

- **sqlx 0.9** ([announcement](https://github.com/launchbadge/sqlx/discussions/4271), [CHANGELOG](https://github.com/launchbadge/sqlx/blob/main/CHANGELOG.md)):
  - `query!`/`query_as!` macros check SQL and types at compile time against a dev database.
  - **Commit the `.sqlx/` offline cache** (`cargo sqlx prepare`) so CI and Docker builds work without a live DB, with `SQLX_OFFLINE=true` in the Dockerfile.
  - 0.9 adds a `sqlx.toml` config file and the `SqlSafeStr` trait, which only accepts `&'static str` queries by default. That is a nice guard against string-built SQL in permission code.
- **Migrations:** plain `.sql` files under `migrations/`, embedded with `sqlx::migrate!()` and run at startup. sqlx takes a Postgres **advisory lock** while migrating, so two app processes starting at once are safe. Use reversible migrations only for dev; production is forward-only.
- **Testing:** `#[sqlx::test]` gives each test a fresh database with migrations applied, which suits the leak-property tests in §2.3 well.
- `PgListener` (part of sqlx) handles LISTEN/NOTIFY (§4.3b, §5).
- **diesel 2.3** (with diesel-async) and **sea-orm 2.0** are viable, but forum queries are hand-tuned SQL with permission joins, FTS functions and arrays, where raw checked SQL is clearer. **Pick sqlx.**

### 4.3 Full-text search on Postgres

**Default: built-in Postgres FTS.** It needs no extra service and no sync pipeline. The index is updated in the same transaction as the post, so permissions and moves can never be stale in the index.

```sql
ALTER TABLE posts ADD COLUMN search tsvector
  GENERATED ALWAYS AS (
    setweight(to_tsvector('english', coalesce(title_for_search, '')), 'A') ||
    setweight(to_tsvector('english', body_text), 'B')
  ) STORED;
CREATE INDEX posts_search_idx ON posts USING GIN (search);

-- query (viewer-filtered in the same statement)
SELECT p.id, t.id, ts_rank_cd(p.search, q) AS rank,
       ts_headline('english', p.body_text, q, 'MaxFragments=2') AS snippet
FROM posts p JOIN topics t ON t.id = p.topic_id,
     websearch_to_tsquery('english', $1) q
WHERE p.search @@ q
  AND p.deleted_at IS NULL
  AND (t.visibility = 'public' OR t.category_id = ANY($2))
ORDER BY rank DESC, p.created_at DESC
LIMIT 20;
```

- **`websearch_to_tsquery`** accepts Google-style input (`"exact phrase" -excluded or`) and **never throws a syntax error** on user input, unlike `to_tsquery`.
- `ts_rank_cd` gives cover-density ranking. `ts_headline` produces highlighted snippets; it is expensive, so run it only on the final page of results.
- **Language:** make the text-search config a site setting (`english`, `simple`, …). Store it as a `regconfig` column per post if multilingual communities matter. `simple` works passably for any language without stemming.
- **Limits:** there is no typo tolerance or fuzzy matching (add `pg_trgm` for "did you mean"/title fuzzy match; it is a core contrib extension available in the official image), only basic BM25-free ranking, and CJK needs extensions. For a 500-user forum (probably under a million posts) GIN-indexed FTS answers in milliseconds.
- Sources: [Postgres docs: text search controls](https://www.postgresql.org/docs/current/textsearch-controls.html), [Prisma: you don't need Elasticsearch](https://www.prisma.io/blog/you-dont-need-elasticsearch-postgres-already-has-full-text-search), [jusdb: tsvector, GIN and ts_rank in production](https://www.jusdb.com/blog/postgresql-full-text-search-tsvector-gin).
- **tantivy 0.26** is a Lucene-like embedded Rust library with BM25, fuzzy queries and facets ([tantivy](https://github.com/quickwit-oss/tantivy), [ParadeDB intro](https://www.paradedb.com/learn/tantivy/introduction)). Adding it means a second index on disk to back up or rebuild, an outbox job to sync it, reindexing on every category move or permission change, and permission filtering re-expressed as a tantivy filter (`category_id` fast field ∈ viewer set). That is a lot of new leak surface for relevance gains a small forum won't notice. **Don't add it in v1.** Revisit only if search quality is a real complaint. (ParadeDB's `pg_search` brings tantivy/BM25 *inside* Postgres, but it requires a non-official Postgres image, which hurts the easy-deploy goal.)

### 4.3b LISTEN/NOTIFY

- **Uses:** (1) cache-generation bumps (§2.2), (2) the live "new post in topic N" / "new notification for user U" events behind SSE (§5), and (3) waking job workers (§4.7).
- **Pattern:** `pg_notify(channel, payload)` inside the writing transaction, with ids only in the payload (well under the 8000-byte limit) and **never content**, because a payload is visible to anything that LISTENs. A single `PgListener` task per app process fans the events out to `tokio::sync::broadcast` channels. sqlx's `PgListener` reconnects automatically. After a reconnect, notifications may have been missed, so the listener bumps `site_gen` (dropping the page cache) and SSE clients get a "resync" event.
- **Scalability caveat:** NOTIFY takes a global lock at commit to keep notifications in commit order. Recall.ai found that this serialised commits at tens of thousands of concurrent writers ([Recall.ai: LISTEN/NOTIFY does not scale](https://www.recall.ai/blog/postgres-listen-notify-does-not-scale), [Simon Willison](https://simonwillison.net/2025/Jul/11/postgres-listen-notify/), counterpoint [DBOS: it actually scales](https://www.dbos.dev/blog/postgres-listen-notify-scalability)). A forum doing a few writes per second is about four orders of magnitude below that. Irrelevant here, but it is a reason to keep NOTIFY to one per transaction.

### 4.4 Markdown and sanitisation

- **comrak 0.55** is the full CommonMark+GFM implementation (tables, task lists, strikethrough, autolinks, footnotes), with an AST you can walk to resolve mentions and quotes. **pulldown-cmark 0.13** is faster and more minimal, streaming events. **Pick comrak** for feature completeness and AST access.
- Always run the output through **ammonia 4.2** with an explicit allow-list: no `style`, no `on*` attributes, `rel="nofollow ugc noopener"` on links, and only `http`/`https`/`mailto` schemes. Never rely on the markdown renderer's "safe mode" alone.
- Cook at write time. Store `raw`, `cooked`, `cook_version`. Resolve mentions, quotes and previews **with the permission rule from §3.2**.
- Add a strict **Content-Security-Policy**: `script-src 'self'`, no inline scripts. Put island props in `<script type="application/json">`, which CSP permits.

### 4.5 Authentication

- **Sessions, not JWT.** Use an opaque 256-bit random token in a `__Host-` cookie that is `HttpOnly; Secure; SameSite=Lax`. Store a **hash** of the token in a `sessions` table with user, created, last_seen and UA. This gives instant revocation ("log out everywhere", bans, password change), which JWTs make painful. Optionally add a short in-memory cache of session→user. tower-sessions 0.15 provides this. A hand-rolled table is simple and avoids an extra dependency's churn: tower-sessions-sqlx-store was last released in 2025-01.
- **CSRF:** `SameSite=Lax` plus an `Origin`/`Sec-Fetch-Site` check on all non-GET requests. Add a per-session token for logged-in forms. This also keeps anon HTML token-free (§2.1).
- **Login methods, in order of priority:**
  1. **Email magic link / one-time code.** Zero passwords, and it doubles as email verification. Use a single-use, 15-min expiry, hashed token. Send a 6-digit code as well, for cross-device logins.
  2. **Passkeys** via webauthn-rs 0.5.5, as the fast path for returning users.
  3. **Password** (argon2 0.6) as an option for people who want it, plus optional TOTP (totp-rs 6).
  4. **External OAuth/OIDC providers, all optional and all off by default.** The admin configures them in the UI (client id and secret stored in the DB) or via env vars:
     - presets for **Discord**, GitHub, Google and GitLab;
     - a **generic OIDC** provider (Authentik, Keycloak, Authelia, Kanidm…) via openidconnect 4.0.1;
     - plain-OAuth2 providers via oauth2 5.0.
     - **Discord specifics:** it publishes an OIDC discovery document and supports the `openid` scope, but its implementation doesn't fully follow the spec ([Discord OIDC config](https://well-known.dev/resources/openid_configuration/sites/discord.com), [discord.food OAuth2 notes](https://docs.discord.food/topics/oauth2)). Treat it as **plain OAuth2** (`identify email` scopes, then `GET /users/@me`). Key the account link on Discord's immutable user id, not username or email.
     - **Model:** a `user_identities(provider, subject, user_id)` table, so one account can link several providers plus email, and no provider is special. Only auto-link an external login to an existing account by email if the provider asserts the email is verified; otherwise require login-then-link. An optional per-provider "restrict signups to members of guild/org X" check is a natural extension, e.g. for a Discord-centred community, but stays optional.
- Rate-limit login and signup with governor/tower_governor, keyed by IP and by email.

### 4.6 Email

- **lettre 0.11.23**, async SMTP with rustls. SMTP is the universal self-hoster interface (Postmark, SES, Mailgun and Fastmail all speak it).
- Send from the job queue (§4.7) with retries. Render per recipient as the recipient (§3.2). Include `List-Unsubscribe`/`List-Unsubscribe-Post` headers. Support reply-by-email later via an inbound webhook.

### 4.7 Background jobs (Postgres queue)

**Recommendation: a hand-rolled Postgres job table** (about 200 lines). It is transactional with the triggering write (the outbox pattern, so there are no lost emails), and it needs no broker.

```sql
CREATE TABLE jobs (
  id bigserial PRIMARY KEY, kind text NOT NULL, payload jsonb NOT NULL,
  run_at timestamptz NOT NULL DEFAULT now(), attempts int NOT NULL DEFAULT 0,
  max_attempts int NOT NULL DEFAULT 10, locked_until timestamptz, last_error text,
  dedupe_key text UNIQUE            -- e.g. 'digest:user:42:2026-10-01'
);
CREATE INDEX jobs_ready ON jobs (run_at) WHERE locked_until IS NULL;

-- claim
UPDATE jobs SET locked_until = now() + interval '5 minutes', attempts = attempts + 1
WHERE id = (SELECT id FROM jobs
            WHERE run_at <= now() AND (locked_until IS NULL OR locked_until < now())
            ORDER BY run_at FOR UPDATE SKIP LOCKED LIMIT 1)
RETURNING *;
```

- Workers race `PgListener` (`NOTIFY jobs` on insert) against a 5–30 s poll interval using `tokio::select!`. Success deletes the row (or moves it to a `jobs_done` table kept for 7 days for the admin UI). Failure sets `run_at = now() + backoff(attempts)` and records `last_error`. Rows that hit max_attempts stay visible in the admin "failed jobs" page with a retry button ([SKIP LOCKED queue pattern](https://aminediro.com/posts/pg_job_queue/), [lobste.rs: choose Postgres queue technology](https://lobste.rs/s/rk3eft/choose_postgres_queue_technology)).
- **Recurring jobs** (digests, pruning, the backup check) come from a tiny in-process scheduler that enqueues with a `dedupe_key`, so restarts or a second process never double-send.
- **Job kinds:** emails and digests, rebake, image processing, read-state flush, pruning of sessions/tokens/notifications, backup status check.
- **Off-the-shelf alternatives**, if a hand-rolled queue isn't wanted:

| Crate | Version | Notes |
|---|---|---|
| **graphile_worker** | 0.13.6 (2026-09-29) | Rust port of Graphile Worker: SKIP LOCKED + LISTEN/NOTIFY, cron, job keys/dedupe, its own schema and migrations ([repo](https://github.com/leo91000/graphile_worker_rs)). The most mature-looking option. |
| apalis-postgres | 1.0.0-rc.9 | apalis framework with a Postgres backend. 1.0 is still a release candidate, and it brings a heavier abstraction (tower services, monitors). |
| underway | 0.2.0 (2025-07) | durable multi-step workflows on Postgres; more than a forum needs |
| sqlxmq | 0.6.0 (2025-05) | simple sqlx-based queue; slower release cadence |
| pgmq | 0.33.7 | client for the pgmq **extension**, which is not in the official Postgres image, so avoid it |

### 4.8 File uploads and images

- Use multipart via axum with `DefaultBodyLimit` per route. Store content-addressed files (`sha256`) under `data/uploads/`. Sniff the real MIME type and reject SVG (or sanitise it heavily).
- For images, decode with `image 0.25`, **strip EXIF/GPS** by re-encoding, and generate thumbnails with `fast_image_resize 6.1` in a background job. Re-encode to WebP, or AVIF if CPU allows.
- **Public uploads** get immutable caching. **Uploads referenced only from private content** are served through an auth-checking handler with `Cache-Control: private` (see §3.2). Serve uploads from a path with `Content-Disposition` and `X-Content-Type-Options: nosniff`. Ideally use a separate cookieless origin, but that's optional for small installs.
- Uploads live in the `forum-data` volume, *not* in Postgres. The backup story (§6.4) must include this volume, not just the database dump. An optional S3 backend can come later.

---

## 5. Frontend

- **Vite 8.3.1 + React 19.3.0**, built as a **multi-entry islands bundle**: one small entry that reads `data-island` attributes and lazy-`import()`s each island's chunk. Anon users load **zero React**. At most they load a tiny vanilla script (≈2–5 KB) for things like relative timestamps, a "log in to reply" prompt, and theme toggling.
- **Bundle budget for logged-in users:** react + react-dom is about 45–60 KB gzipped. Aim for less than about 100 KB gzipped for the initial logged-in JS, lazy-load the composer's editor and markdown preview, and avoid heavyweight UI kits.
- **Preact 11.0.0** with the `preact/compat` alias cuts the runtime to about 4–5 KB and is a one-line Vite alias. It is worth considering since only islands use it. Stick with React unless bundle size proves to matter. The alias can be flipped later as long as no React-only libraries are adopted.
- **htmx** (2.0.11 stable. 4.0.0 shipped 2026-08-28 with a fetch-based rewrite and built-in morphing, but is on the `next` npm tag until early 2027; see [InfoQ](https://www.infoq.com/news/2026/09/htmx-4-released/), [dev.to migration notes](https://dev.to/morellodev/htmx-4-whats-new-what-breaks-why-its-not-latest-4bld)) fits the "server returns HTML fragments" model very naturally. A reasonable split is **htmx for simple enhancements** (like buttons, load-more, infinite scroll) **plus React only for the composer and notifications**. Given the stated React preference, the default plan stays React islands that *consume server-rendered HTML fragments*. Revisit if the React surface stays tiny.
- Data fetching inside islands: TanStack Query 5 is fine. Share API types from Rust via `ts-rs` 12.
- **Real-time updates: SSE.**
  - `axum::response::sse::Sse` streams events. The server keeps a `tokio::sync::broadcast` channel per active topic, plus one per user for notifications. These are fed by the single `PgListener` task (§4.3b), so events only fire after commit and also work if writes come from a separate worker process.
  - Events are small (`{"topic":123,"post":456,"gen":789}`). The client then fetches the rendered fragment, which is cacheable for public topics and permission-checked for private ones. That way **no content is pushed over the stream** and permission checks stay in one place.
  - SSE works through HTTP/2 multiplexing, auto-reconnects with `Last-Event-ID`, and passes proxies more easily than WebSockets. Anon readers can optionally get a cheap "new posts available" stream, or just poll an ETag'd endpoint.
  - WebSockets are unnecessary: there is no high-frequency client→server traffic, and chat is not in scope.
  - Configure Caddy/nginx not to buffer `text/event-stream`, and send heartbeats every 15–30 s.

---

## 6. Deployment (open-source, docker compose first)

The target user is a non-expert admin with a 1–2 GB VPS and a domain name. The aim: **`git clone` (or download one `compose.yaml` + `.env.example`), edit 3–5 values, run `docker compose up -d`, then open `https://forum.example.org/setup`.**

### 6.1 The artefact

- **One Rust binary**:
  - Built as static musl for amd64 and arm64.
  - Embeds the Vite `dist/` output with **memory-serve 2.4** (compressed at build time, served with ETag and immutable caching) or rust-embed 8.12.
  - Embeds the SQL migrations (`sqlx::migrate!()`) and the askama templates.
  - Result: about 15–30 MB. This is the main reason **not** to embed V8.
- **One image:** `ghcr.io/<org>/forum:<semver>`, a multi-stage build ending `FROM gcr.io/distroless/static` (or `scratch` + CA certs), multi-arch, with a `HEALTHCHECK` hitting `/healthz` (which checks the DB). Also publish the bare binary and a `.deb`/systemd unit for non-Docker users, who bring their own Postgres.
- **Subcommands:**
  - `forum serve` (the default);
  - `forum migrate`;
  - `forum backup [--out path]` (pg_dump-compatible dump plus an uploads tarball);
  - `forum restore <path>`;
  - `forum create-admin <email>` (a fallback to the web setup);
  - `forum rebake`;
  - `forum check` (validates config, DB connectivity and SMTP).

### 6.2 Reference `compose.yaml`

```yaml
services:
  forum:
    image: ghcr.io/ORG/forum:1
    restart: unless-stopped
    env_file: .env
    environment:
      DATABASE_URL: postgres://forum:${POSTGRES_PASSWORD}@db/forum
    volumes: [forum-data:/data]          # uploads, instance id, local backups
    depends_on: { db: { condition: service_healthy } }
    networks: [internal, web]
  db:
    image: postgres:18                   # pin the major version
    restart: unless-stopped
    environment: { POSTGRES_USER: forum, POSTGRES_DB: forum, POSTGRES_PASSWORD: "${POSTGRES_PASSWORD}" }
    command: ["postgres", "-c", "config_file=/etc/postgresql/postgresql.conf"]
    volumes:
      - pg-data:/var/lib/postgresql      # PG18+ convention (see §4.1)
      - ./postgres.conf:/etc/postgresql/postgresql.conf:ro
    shm_size: 256mb
    healthcheck: { test: ["CMD-SHELL", "pg_isready -U forum"], interval: 5s, retries: 10 }
    networks: [internal]                 # never published
  caddy:
    image: caddy:2
    restart: unless-stopped
    ports: ["80:80", "443:443", "443:443/udp"]
    environment: { FORUM_DOMAIN: "${FORUM_DOMAIN}" }
    volumes: [./Caddyfile:/etc/caddy/Caddyfile:ro, caddy-data:/data, caddy-config:/config]
    networks: [web]
  backup:
    image: prodrigestivill/postgres-backup-local:18
    restart: unless-stopped
    environment:
      { POSTGRES_HOST: db, POSTGRES_DB: forum, POSTGRES_USER: forum, POSTGRES_PASSWORD: "${POSTGRES_PASSWORD}",
        SCHEDULE: "@daily", BACKUP_KEEP_DAYS: 7, BACKUP_KEEP_WEEKS: 4, BACKUP_KEEP_MONTHS: 6 }
    volumes: [./backups:/backups]
    networks: [internal]
volumes: { forum-data: {}, pg-data: {}, caddy-data: {}, caddy-config: {} }
networks: { internal: { internal: true }, web: {} }
```

The Caddyfile is three lines: `{$FORUM_DOMAIN} { encode zstd gzip; reverse_proxy forum:8080 }`. Caddy 2.11 obtains and renews Let's Encrypt or ZeroSSL certificates automatically, handles HTTP→HTTPS, and does HTTP/3. SSE works through Caddy's `reverse_proxy` out of the box, because it flushes `text/event-stream` responses immediately. Send `X-Accel-Buffering: no` too, for admins who put nginx in front instead. The **forum must trust `X-Forwarded-For` only from the Caddy network**, which matters for rate limiting. For admins who already run a reverse proxy, the docs show the same setup without the caddy service (expose `forum:8080` instead).

**Optional CDN:** document a Cloudflare Cache Rule ("cache eligible when the cookie does not contain `__Host-fsid`") as an add-on. Nothing in the app depends on it (§2.2).

### 6.3 Configuration: minimal and env-first

- **Required** in `.env`:
  - `FORUM_DOMAIN` (and from it `FORUM_BASE_URL`);
  - `POSTGRES_PASSWORD`, which an install script can generate with `openssl rand`;
  - SMTP settings: `SMTP_URL=smtps://user:pass@smtp.example.com:465` (one URL, parsed by lettre's `from_url`), and `MAIL_FROM`.
- **Everything else has a default or lives in the DB** (admin UI): site name, logo, categories, groups, permissions, login providers (Discord etc.), registration policy (open / invite-only / approval), rate limits.
- Env values take precedence and are shown as "locked by environment" in the admin UI.
- Use `figment` or plain serde to merge defaults, an optional `forum.toml` and `FORUM_*` env vars.
- `forum check` and the setup wizard **send a test email**. Without working SMTP, magic-link login cannot work, so the wizard makes this the first thing it verifies. It falls back to a password for the first admin if SMTP is broken, so the admin isn't locked out.

### 6.4 First-run setup

1. On startup, if no admin exists, the app generates a **one-time setup token**, stores its hash, and prints a banner to the logs: `Visit https://forum.example.org/setup?token=XXXX to finish setup`. Read it with `docker compose logs forum`. Without the token, `/setup` returns 404, so a random visitor can't claim a freshly deployed instance before the owner does. This is a common hole in self-hosted apps.
2. The **`/setup` wizard** walks through these steps:
   1. create the admin account (email + password or passkey);
   2. confirm the site name, base URL and contact email;
   3. send an **SMTP test** and show the result;
   4. choose the registration policy;
   5. optionally enable login providers, with a Discord preset showing the redirect URL to paste into Discord's developer portal;
   6. choose a starter category layout: one public "General" category plus a members-only "Members" category, with an example group-gated category template.
3. The setup token is consumed, and `/setup` returns 404 from then on. `forum create-admin <email>` stays as a CLI escape hatch for recovery: it prints a one-time login link.

### 6.5 Migrations and upgrades

- App schema migrations run **automatically at startup** under sqlx's advisory lock, inside a transaction where possible. They are forward-only.
- Before applying any pending migration, the app runs a **pre-migration dump** (`pg_dump -Fc` to `/data/backups/pre-<version>.dump`). So the image includes `pg_dump`/`pg_restore` binaries matching the server major, or calls the backup sidecar.
- The app **refuses to start** if the DB schema is newer than the binary (a downgrade) and prints the restore command.
- Release notes flag any migration expected to take more than a few seconds.
- **Postgres major upgrades** (18→19 someday) are a separate, documented, manual procedure (§4.1). The app's version-check refuses to treat an empty DB as a fresh install when `/data` says it isn't.

### 6.6 Backups a non-expert can manage

| Tier | What | How | Restore point |
|---|---|---|---|
| **Default (on)** | Nightly `pg_dump` with 7 daily, 4 weekly and 6 monthly copies via hard-link rotation, plus an uploads snapshot | `prodrigestivill/postgres-backup-local` sidecar ([repo](https://github.com/prodrigestivill/docker-postgres-backup-local); `SCHEDULE`, `BACKUP_KEEP_*` env vars), or the app's own scheduled `forum backup`. Pre-migration dumps as well. | Up to 24 h of loss |
| **Recommended** | Off-site copy of `./backups` + the uploads volume | Document one restic/rclone container or cron line pushing to B2/S3/SFTP; or `forum backup` with `BACKUP_S3_URL` set | Same, but survives losing the VPS |
| **Advanced (opt-in profile)** | Continuous WAL archiving + weekly full / daily differential, encrypted, to S3 | **pgBackRest 2.59** (released 2026-09-27) as a compose profile (`docker compose --profile pitr up`), or wal-g 3.0.9 | Point-in-time, seconds of loss |

- At forum scale a dump takes seconds and the **daily dump plus an off-site copy is the right default**. PITR via pgBackRest is powerful but needs `archive_command` config, a repository and stanza setup, and restore drills, which is too much to impose on every admin ([pgBackRest user guide](https://pgbackrest.org/user-guide.html), [Data Egret: pgBackRest PITR in Docker](https://dataegret.com/2025/12/pgbackrest-pitr-in-docker-a-simple-demo/), [pgBackRest + S3 PITR](https://mysticmind.dev/postgresql-point-in-time-recovery-with-pgbackrest-and-s3-compatible-storage/)).
- **Make backups visible:** the admin dashboard shows "last successful backup: 3 h ago (12 MB)" and turns red after 36 h. `forum restore <file>` is a single documented command, and the docs include a **restore drill** ("spin up a second compose project from the backup").
- **Uploads:** the dump does not contain uploaded files, so `forum backup` produces `{db.dump, uploads.tar}` together. Alternatively, uploads are content-addressed and append-mostly, so `rsync`/restic of the volume is cheap.

### 6.7 Second community

Run **a second compose project** (or a second `forum` + `db` pair in the same project) with its own domain, volumes and DB. Caddy routes by hostname. On a 2 GB VPS two instances fit comfortably: each app is about 30–60 MB RSS, and the two can share one Postgres server with two databases and two roles. **Multi-tenancy inside one process is not worth it** at this scale. It adds a tenant key to every query, every cache key and every permission check, which is a whole new leak class.

### 6.8 Observability

`tracing` with JSON or pretty logs to stdout (read with `docker compose logs`), an optional `/metrics` (Prometheus) endpoint, and an admin "health" page showing: version, migrations, DB size, anon-cache hit rate, p95 render time, job-queue depth and failures, last backup, SMTP status, and disk free on `/data`.

---

## 7. Open questions for the next research step

1. Do we want admin-customisable templates/themes? If so, minijinja instead of askama.
2. Quote-fork policy: hard refuse versus moderator-override when quoting from a more private category into a less private one (§3.1). Also, should quotes be live references (recommended) or copied text?
3. Read-tracking granularity: per post versus a per-topic high-water mark. Flat chronological threads make the high-water mark natural and cheap.
4. Is reply-by-email or ActivityPub federation in scope? Both widen the permission surface (§3.2).
5. Should DM search exist in v1, or is it deferred?
6. Should restricting sign-up to members of a Discord guild or GitHub org be a built-in option?

---

## Sources

- Discourse crawler layout: https://github.com/discourse/discourse/blob/main/app/views/layouts/crawler.html.erb
- What Googlebot sees on Discourse: https://meta.discourse.org/t/what-googlebot-sees-when-crawling-discourse/212726
- Discourse noscript view discussion: https://meta.discourse.org/t/add-a-get-option-to-enable-noscript-view-of-a-page/33823
- Evil Trout on SEO for JS apps: https://fishtank.eviltrout.com/t/adding-support-for-search-engines-to-your-javascript-applications/22
- Discourse defaults (anon cache): https://github.com/discourse/discourse/blob/main/config/discourse_defaults.conf
- Discourse anon cache perf: https://review.discourse.org/t/feature-improve-performance-of-anonymous-cache/5518
- Discourse anon cache poisoning CVE-2024-47773: https://github.com/discourse/discourse/security/advisories/GHSA-58vv-9j8h-hw2v
- Discourse login-required leak: https://github.com/discourse/discourse/security/advisories/GHSA-v3h7-c287-pfg9
- Discourse digest leak: https://github.com/discourse/discourse/security/advisories/GHSA-q9jp-xv4g-328f
- Discourse user-action leak: https://github.com/discourse/discourse/security/advisories/GHSA-824f-66wh-xx3g
- Discourse group PM leak: https://github.com/discourse/discourse/security/advisories/GHSA-vm3x-w6jm-j9vv
- discourse-reactions leak: https://github.com/discourse/discourse-reactions/security/advisories/GHSA-9358-hwg5-jrmh
- Discourse read-state leak fix: https://github.com/discourse/discourse/commit/aed65ec16d38886d7be7209d8c02df4ffd4937a4
- Discourse onebox bypass (HackerOne): https://hackerone.com/reports/312647
- Discourse CVE list: https://vulners.com/search/vendors/discourse/products/discourse
- Discourse Private Topics plugin: https://meta.discourse.org/t/private-topics-plugin/268646
- Flarum mentions CVE-2023-22487: https://github.com/flarum/framework/security/advisories/GHSA-22m9-m3ww-53h3
- Flarum frontend docs: https://docs.flarum.org/extend/frontend/
- Flarum crawler pagination: https://github.com/flarum/framework/issues/1310
- NodeBB vulnerabilities (2026): https://thehackernews.com/2026/07/nodebb-patches-eight-ai-found-flaws.html , https://www.aikido.dev/blog/eight-high-severity-vulnerabilities-nodebb
- Lobsters logged-out cache bug: https://github.com/lobsters/lobsters/issues/2209
- HN on Cloudflare / single server: https://news.ycombinator.com/item?id=23875692 , https://danielpetrica.com/surviving-hacker-news-front-page/
- Old vs new Reddit: https://redcurate.com/blog/old-reddit , https://www.cole-k.com/2026/07/21/reddit/
- ssr_rs: https://crates.io/crates/ssr_rs , https://github.com/Valerioageno/ssr-rs , https://medium.com/@valerioageno95/step-by-step-guide-to-server-side-render-react-with-rust-640519ec48dc
- MDN Cache-Control: https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Cache-Control
- Cache-Control for Civilians: https://csswizardry.com/2019/03/cache-control-for-civilians/
- Cloudflare cookies & cache: https://stackharbor.com/en/knowledge-base/cfops-cookies-cache-bypass/ , https://stackharbor.com/en/knowledge-base/cfops-cookies-through-cloudflare/ , https://developers.cloudflare.com/cache/how-to/cache-rules/settings/ , https://developers.cloudflare.com/cache/concepts/cache-control/
- Caddy cache-handler: https://github.com/caddyserver/cache-handler , https://caddy.community/t/some-questions-regarding-the-cache-handler-souin-module-and-wordpress/26541
- nginx caching: https://docs.nginx.com/nginx/admin-guide/content-cache/content-caching/ , https://www.getpagespeed.com/server-setup/nginx/nginx-proxy-cache-microcaching
- sqlx 0.9: https://github.com/launchbadge/sqlx/discussions/4271 , https://github.com/launchbadge/sqlx/blob/main/CHANGELOG.md
- axum versions: https://crates.io/crates/axum/versions
- tantivy: https://github.com/quickwit-oss/tantivy , https://www.paradedb.com/learn/tantivy/introduction
- htmx 4: https://www.infoq.com/news/2026/09/htmx-4-released/ , https://dev.to/morellodev/htmx-4-whats-new-what-breaks-why-its-not-latest-4bld
- Postgres FTS: https://www.postgresql.org/docs/current/textsearch-controls.html , https://www.prisma.io/blog/you-dont-need-elasticsearch-postgres-already-has-full-text-search , https://www.jusdb.com/blog/postgresql-full-text-search-tsvector-gin
- LISTEN/NOTIFY scaling: https://www.recall.ai/blog/postgres-listen-notify-does-not-scale , https://simonwillison.net/2025/Jul/11/postgres-listen-notify/ , https://www.dbos.dev/blog/postgres-listen-notify-scalability
- Postgres job queues: https://github.com/leo91000/graphile_worker_rs , https://aminediro.com/posts/pg_job_queue/ , https://lobste.rs/s/rk3eft/choose_postgres_queue_technology
- Postgres tuning: https://pgtune.leopard.in.ua/ , https://www.enterprisedb.com/postgres-tutorials/how-tune-postgresql-memory , https://bun.uptrace.dev/postgres/performance-tuning.html , https://usavps.com/blog/post/postgresql-production-tuning-vps/
- Postgres 18 Docker PGDATA change: https://rdiachenko.com/posts/databases/postgresql/postgres-18-docker-silently-ignores-your-named-volume/ , https://hub.docker.com/_/postgres , https://www.nite07.com/en/posts/postgres-docker-18-migration/ , https://gist.github.com/LukaPrebil/322831c0aaed075ebffbb40a3ba1889f
- Backups: https://github.com/prodrigestivill/docker-postgres-backup-local , https://pgbackrest.org/user-guide.html , https://dataegret.com/2025/12/pgbackrest-pitr-in-docker-a-simple-demo/ , https://mysticmind.dev/postgresql-point-in-time-recovery-with-pgbackrest-and-s3-compatible-storage/
- Discord OAuth/OIDC: https://well-known.dev/resources/openid_configuration/sites/discord.com , https://docs.discord.food/topics/oauth2 , https://discord.com/developers/docs/social-sdk/authentication.html
- Crate/package versions: crates.io API (`https://crates.io/api/v1/crates/<name>`), npm registry (`https://registry.npmjs.org/<pkg>/latest`), GitHub releases API (Caddy v2.11.4, pgBackRest 2.59.2, wal-g v3.0.9), Docker Hub tags (postgres 18.6), queried 2026-10-01.
