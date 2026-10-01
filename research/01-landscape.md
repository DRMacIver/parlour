# 01 — Landscape of self-hostable forum software (as of Oct 2026)

Context: we are designing a new minimal, fast, "somewhat oldschool" forum (Rust backend, React frontend). It will be open source, deployable with `docker compose`. It needs public and private threads, and the logged-out public view has to be served very efficiently. The first community is a ~500-person Discord server moving partly onto the forum. They want **flat threads**, **members-only categories gated by group**, and **DMs**.

For each project this file records the stack, how hard it is to install and maintain (Docker, number of services, upgrade pain, resources), the licence, the rendering approach (especially for anonymous users and crawlers), the permission model, and what people praise and complain about.

Unverified details are marked *(unverified)*.

---

## 1. Summary table

| Project | Stack | Licence | Services in a typical Docker deploy | Rendering for anonymous users | Private sections | DMs | Status |
|---|---|---|---|---|---|---|---|
| **Discourse** | Ruby on Rails + Ember.js, Postgres, Redis, Sidekiq | GPL-2.0 | 1 big "launcher" container (Rails+Sidekiq+PG+Redis+nginx); 2 GB RAM recommended | SPA for browsers; separate no-JS "crawler" layout picked by User-Agent; Redis anonymous page cache | Category security per group (See / Reply / Create) | Yes (PMs, group PMs, chat) | Very active |
| **Flarum** | PHP 8.3 (Laravel components) + Mithril SPA | MIT | No official image; community images + MySQL/MariaDB | SPA; basic server-rendered content for no-JS/crawlers | Tags extension, permissions per group | `flarum/messages` (official, 2.0 beta) | 2.0 still RC (rc.8) |
| **NodeBB** | Node.js 22, MongoDB / Redis / Postgres | GPL-3.0 | 2 (nodebb + mongo, or + postgres/redis) | First load rendered on server (Benchpress), then client-side "ajaxify" navigation | Per-category privileges per group | Yes (chats) | Active; v4 added ActivityPub |
| **phpBB** | PHP, MySQL/PG/SQLite | GPL-2.0 | No official Docker; Bitnami image deprecated | Classic server-rendered pages | Very granular ACL (Yes/No/Never, roles) | Yes | 3.3 stable; 4.0 alpha (Symfony 6.4) |
| **MyBB** | PHP, MySQL/PG/SQLite | LGPL-3.0 | No official compose *(unverified)* | Classic server-rendered | Per-forum, per-group | Yes | 1.8 stable; 1.9 ~88% in mid-2026 |
| **SMF** | PHP | BSD-3 | Traditional LAMP | Classic server-rendered | Membergroups + board access | Yes | 2.1 stable; 3.0 alpha (PHP 8.4) |
| **Vanilla OSS** | PHP | GPL-2.0 | — | — | Roles/categories | Yes | **Open-source edition discontinued** |
| **XenForo** | PHP, MySQL | Proprietary ($195+) | LAMP | Classic server-rendered | Node permissions per group | Yes | Commercial, active |
| **Misago** | Django, Postgres, Redis (+React, being removed) | GPL-2.0 | 4 (misago, postgres, redis, nginx) via `misago_docker` | Mostly server-rendered Django, plus React islands | Category permissions; private threads | Private threads | Active, "perpetual beta" |
| **Lemmy** | Rust (actix, diesel) + lemmy-ui (Inferno, isomorphic SSR) | AGPL-3.0 | 5 (nginx, lemmy, lemmy-ui, pict-rs, postgres) | Rendered on server by a Node frontend process | Private communities only from 1.0 (still beta) | Yes | 1.0 in beta |
| **Mbin / PieFed** | PHP Symfony / Python Flask | AGPL-3.0 | Mbin: PHP, Node, Redis, Postgres, RabbitMQ, AMQProxy, Supervisor, nginx | Server-rendered | Limited | Yes | Active |
| **Lobsters** | Rails, SQLite (since 2026), Solid Queue | BSD-3 | Single VPS (Hatchbox); no official Docker *(unverified)* | **Static HTML file cache served by Caddy to logged-out users; no JS for logged-out** | None (public link aggregator) | Yes | Active |
| **Tildes** | Python Pyramid, Postgres, Redis, intercooler.js | AGPL-3.0 | Not designed for third-party installs *(unverified)* | Server-rendered, minimal JS | Groups (public) | Yes | Slow |
| **Hacker News** | Arc → Clarc on SBCL (closed source) | n/a | n/a | Logged-out users served from cache | n/a | n/a | UI reference |
| **Fider** | Go + React | AGPL-3.0 | 2 (fider + postgres) | **SPA for browsers; React rendered on the server in embedded V8 for crawlers only** | Private site mode | No | Active (feedback board, not a forum) |
| **Talkyard** | Scala/Play + React, Postgres, Redis, ElasticSearch | AGPL-3.0 | 5+ containers; 2 GB RAM minimum | Server-rendered React | Private categories | Yes | Active |
| **Apache Answer** | Go + React | Apache-2.0 | **1 container** (SQLite by default) | SPA (Q&A) | Plugins | No | Active |
| **Storyden** | Go + Next.js 16 / React 19 | MPL-2.0 on GitHub (PRODUCT.md says MIT) | **1 container**, SQLite by default (Postgres optional) | Next.js | Additive RBAC roles | *(unverified)* | Active |
| **cerca** | Go, SQLite, plain HTML/CSS | AGPL-3.0 | Single binary + nginx | Server-rendered, no JS | **Per-thread public / private (logged-in only)** | No | Active, small |
| **freedit** | Rust, embedded fjall KV store | MIT | Single binary | Server-rendered, fully works without JS | "Inns" (can be private) | E2E-encrypted DMs | Active, small |
| **Waterhole** | Laravel + Hotwire (Turbo/Stimulus) | Source-available, $299/site | LAMP | Server-rendered + Turbo; works without JS | Private channels via groups | *(unverified)* | Active |
| **NimForum** | Nim (Karax SPA), SQLite | MIT | Single binary | SPA | Limited | No | Low activity |
| **agreper** | Flask, SQLite | AGPL-3.0 | Single process | No JS | No | No | Dormant since 2024 |

---

## 2. The mainstream options

### 2.1 Discourse

**Stack and hosting.** Rails + Ember.js, PostgreSQL, Redis and Sidekiq. Everything runs in one Docker container built by Discourse's `launcher` tool. The official minimum is 1 GB RAM plus swap. In practice, 2 GB RAM and 2 cores are the recommended starting point, and 4 GB is comfortable. The stack idles at about 1.4–1.8 GB. Sources: [meta: how much RAM](https://meta.discourse.org/t/how-much-ram-is-required-for-discourse/157730), [onout.org sizing guide 2026](https://onout.org/discourse/self-hosting-production/).

**Rendering.**
- Browsers get the full Ember SPA.
- Clients whose User-Agent matches a crawler list get a separate server-rendered, no-JS layout (`crawler.html.erb`). Sources: [crawler layout source](https://github.com/discourse/discourse/blob/main/app/views/layouts/crawler.html.erb), [flexible crawler detection commit](https://github.com/discourse/discourse/commit/f6fdc1ebe81652be07e8c2c12b59812305de1ba5).
- Logged-out requests can be served from an **anonymous cache** of whole responses in Redis. The topic TTL defaults to 60 s, and an entry is only stored after it has been requested N times.
- One benchmark went from 25 to 380 req/s when anonymous caching applied. Sources: [anon cache perf review](https://review.discourse.org/t/feature-improve-performance-of-anonymous-cache/5518), [Middleware::AnonymousCache](https://www.rubydoc.info/github/discourse/discourse/Middleware/AnonymousCache/Helper).
- This cache has had cache-poisoning security advisories: [GHSA-58vv-9j8h-hw2v](https://github.com/discourse/discourse/security/advisories/GHSA-58vv-9j8h-hw2v).

**Permissions.** Each category has an access list of groups, and each group gets one of three levels: *See*, *Reply* or *Create*. A category becomes private by removing `everyone`. Discourse also has trust levels, PMs, group inboxes and chat. Source: [Understanding groups and category permissions](https://meta.discourse.org/t/understanding-groups-and-category-permissions-security-settings/87678).

**Discord angle.** Discord login is built into core. The chat-integration plugin can post new topics to Discord. There is no official way to import Discord history. Sources: [Discord OAuth](https://meta.discourse.org/t/discord-oauth2-plugin/67174), [chat integration](https://meta.discourse.org/t/discourse-chat-integrations/66522), [migration from chat apps not supported](https://meta.discourse.org/t/is-migration-from-other-chat-apps-supported/236724).

**Praise.** It has good search, live Markdown preview, email integration and solid moderation. It is quick to spin up on a VPS ([HN 2025 thread](https://news.ycombinator.com/item?id=42513894), [HN 37591926](https://news.ycombinator.com/item?id=37591926)).

**Complaints.**
- **Heavy JavaScript and nothing without JS.** Users call it "very slow and bloated" and report 8-second page loads ([HN 37591926](https://news.ycombinator.com/item?id=37591926); [HN Nimforum thread](https://news.ycombinator.com/item?id=28329672); [meta: JS on Android](https://meta.discourse.org/t/the-state-of-javascript-on-android-in-2015-is-poor/33889/print)).
- **Infinite scroll and Ctrl+F hijacking.** The scrollbar becomes meaningless and long threads get worse ([meta: infinite scrolling is a total pain](https://meta.discourse.org/t/infinite-scrolling-is-a-total-pain/225532), [meta: time to reconsider infinite scroll](https://meta.discourse.org/t/time-to-reconsider-infinite-scroll/401984?tl=en)). Discourse's own staff and fans defend it, pointing to the timeline and the fact that only 20 posts are loaded at a time.
- **Look and feel.** Users describe it as "airport bathroom" sterile and hard to customise ([HN 37591926](https://news.ycombinator.com/item?id=37591926)).
- **Admin burden.** `./launcher rebuild` failures, plugin breakage on upgrade, Docker version pinning, and no way to run on shared PHP hosting ([launcher rebuild failures](https://meta.discourse.org/t/launcher-fails-rebuild-and-leaves-container-not-running/151013), [plugin broke site](https://meta.discourse.org/t/docker-plugin-update-broke-site-during-latest-update/225031), [Docker version error](https://meta.discourse.org/t/issue-after-docker-update-unsupported-version-error/344700)).
- **Engagement nudges.** It turns on digest emails by default ([HN 42513894](https://news.ycombinator.com/item?id=42513894)).

### 2.2 Flarum

**Stack and requirements.**
- PHP 8.3+ with Composer and shell access. Databases: MySQL, MariaDB, PostgreSQL or SQLite (in 2.x).
- The frontend is a Mithril SPA. Source: [install docs](https://docs.flarum.org/2.x/install/).
- There is **no official Docker image**, only community ones such as crazy-max and mondedie ([search](https://github.com/crazy-max/docker-flarum)).
- 2.0 has been in release candidates for a long time (rc.8 in Sept 2026) ([rc.8](https://discuss.flarum.org/d/39746-flarum-200-rc8-released)).
- MIT licence.

**Rendering.** An SPA. Server-rendered pages include a basic content block for no-JS users and crawlers. That block also leaks loader text into search snippets ([PR discussion](https://github.com/mrkcntrmn/flatrate-flarum-forum-navigation/pull/22)).

**Permissions.** Tags with restricted permissions per group. DMs come from the official `flarum/messages` extension, which is still a 2.0 beta ([extension page](https://flarum.org/extension/flarum/messages)).

**Praise.** It looks modern, works well on mobile and runs on shared hosting ([HN 42513894](https://news.ycombinator.com/item?id=42513894)).

**Complaints.**
- It took 5+ years to leave beta, and 2.0 is again slow to ship.
- Core features live in extensions, so extensions get abandoned or broken by API changes ([abandoned extensions thread](https://discuss.flarum.org/d/34585-abandoned-extensions), [rob006 critique (2019, partly resolved by 2022)](https://rob006.net/en/blog/a-few-words-about-flarum/)).
- Users say it is "slow to use, running entirely off JavaScript" ([I do not like Flarum](https://discuss.flarum.org/d/30004-i-do-not-like-flarum)).
- **Telling detail:** Flarum's original creator, Toby Zerner, left and built **Waterhole** on Laravel + Hotwire "without any of the complexity of a JavaScript framework" ([Introducing Waterhole](https://waterhole.dev/community/posts/2-introducing-waterhole)).

### 2.3 NodeBB

**Stack.** Node.js 22+, with MongoDB 5+, Redis 7.2+ or PostgreSQL. 512 MB RAM minimum. GPL-3.0. Sources: [README](https://github.com/NodeBB/NodeBB), [requirements](https://docs.nodebb.org/installing/os/).

**Install.** The official `docker-compose.yml` runs nodebb + mongo, with optional redis and postgres profiles.

**Rendering.** The first page load is rendered on the server with Benchpress templates. After that, the client-side "ajaxify" router takes over, and websockets drive real-time updates ([templates docs](https://nodebb.readthedocs.io/en/latest/themes/templates.html)).

**Federation.** v4 (Jan 2025) made ActivityPub federation core ([TechCrunch](https://techcrunch.com/2025/01/21/forum-software-nodebb-joins-the-fediverse/)).

**Permissions.** Fine-grained privileges per category and per group (find, read, topics:create, …). Adding a group to "read" makes the category whitelisted ([community thread](https://community.nodebb.org/topic/13357/how-do-i-make-a-category-read-only-and-private-categories), [privileges docs](https://docs.nodebb.org/activitypub/privileges/)). Has chats/DMs.

**Complaints.** Plugins break on upgrade. The webpack build is slow (around 50 s). Low-RAM VPSes crash during plugin installs ([Cloudron forum](https://forum.cloudron.io/topic/7045/nodebb-reported-to-be-buggy-not-having-control-over-cloudron-s-release-pace-could-be-a-problem), [Capterra](https://www.capterra.com/p/161236/NodeBB/reviews/)). One 2024 description called it "the best of a bad bunch if a Discourse-like forum is a must" ([itsfoss/selfhosted roundup](https://itsfoss.com/open-source-forum-software/)).

### 2.4 The classic PHP boards: phpBB, MyBB, SMF, XenForo, Vanilla

**phpBB.**
- GPL-2.0. 3.3 is stable.
- 4.0.0-a1 (Sept 2025) moved to Symfony 6.4 and PHP 8.1, and adds web push, @mentions and Turnstile ([phpBB blog](https://blog.phpbb.com/2025/09/27/phpbb-4-0-0-a1-the-first-alpha-release-is-here/)).
- There is **no official Docker image**: the project recommends manual installs, and Bitnami's image is deprecated ([phpBB Docker thread](https://www.phpbb.com/community/viewtopic.php?t=2655829)).
- Permissions are the most expressive of any project here, and the most confusing. Each permission is Yes, No or Never (Never overrides Yes). There are admin, moderator, forum and user permission types, preset roles, and exceptions for founders and global mods ([KB](https://www.phpbb.com/support/docs/en/3.1/kb/article/phpbb3-permissions/), [bbarchive guide](https://www.bbarchive.com/forum-permissions-in-phpbb/)).

**MyBB.** LGPL-3.0. 1.9 adds Twig themes and Composer and was about 88% done in mid-2026 ([milestone](https://github.com/mybb/mybb/milestone/22), [blog](https://blog.mybb.com/2025/12/22/warm-up-to-mybb-1-9/)). Praised for running on any shared host ([HN 42513894](https://news.ycombinator.com/item?id=42513894)).

**SMF.** BSD-style licence. 3.0 is in alpha and needs PHP 8.4 ([SMF 3.0 updates](https://blogs.simplemachines.org/dev/594037/SMF+3.0+updates.html)).

**XenForo.** Proprietary: $195 for the first year of updates, then paid renewals, and add-ons cost $60–350 each. MySQL only. Widely regarded as the most polished classic forum, with permissions admins find easy. The complaints are price and closed source ([pricing](https://xenforo.com/purchase/self-hosted), [Capterra](https://www.capterra.com/p/238692/XenForo/)).

**Vanilla.** Higher Logic **discontinued the open-source edition**, citing legacy code after the PHP 8.1 upgrade ([An Update on Vanilla Open Source](https://open.vanillaforums.com/discussion/39624/an-update-on-vanilla-open-source)).

**Common thread.** All of these are fully server-rendered, fast to first paint and indexable. They are criticised as dated ("janky PHP"), with BBCode, weak mobile UX and extension or mod ecosystems that break across major versions. Major versions take years: phpBB 4, MyBB 1.9 and SMF 3.0 are all still pre-release in 2026.

### 2.5 Misago (Python/Django)

GPL-2.0, Django + Postgres + Redis. Built-in features include private threads, category permissions per rank/role, polls, attachments, OAuth and a moderation queue.

**Install.** The production install is `misago_docker`: Misago + Postgres + Redis + nginx, with backup and restore wizards ([misago_docker announcement](https://misago-project.org/t/misago-docker-is-now-available-for-easy-misago-setup-and-maintenance/726/)).

**Direction.** Release 0.39 began **removing React.js from the codebase**, moving back to server rendering ([releases](https://github.com/rafalp/Misago/releases)). Another project moving away from the SPA.

---

## 3. Link aggregators and federated "threadiverse"

### 3.1 Lemmy (Rust)

**Stack.** Rust backend (actix-web, diesel, Postgres). AGPL-3.0. The UI is `lemmy-ui`, written in Inferno, which renders on the server through a Node process ([repo](https://github.com/LemmyNet/lemmy)).

**Install.** The official compose has **5 services**: nginx proxy, lemmy, lemmy-ui, pict-rs (images) and postgres ([docker-compose.yml](https://github.com/LemmyNet/lemmy/blob/main/docker/docker-compose.yml)).

**Private communities.** Only arrive in **1.0**, still beta as of Aug 2026 (beta.2). They are visible to approved followers only, alongside unlisted and local-only modes ([1.0 testing](https://join-lemmy.org/news/2025-12-24_-_Lemmy_Release_v0.19.15_and_Testing_for_1.0), [beta.2](https://lemmy.ml/post/52603605)).

**Performance lessons.** A Rust backend did not by itself make Lemmy fast.
- During the 2023 Reddit exodus, page loads reached seconds. One query pulled 4 billion rows a day. The fixes were indexes and query changes ([phiresky: Improving Lemmy performance](https://phiresky.github.io/blog/2023/improving-lemmy-performance/)).
- Admins report Postgres OOM kills ([issue #4452](https://github.com/LemmyNet/lemmy/issues/4452)).
- One admin saw load climb from 1 to 12 and switched to PieFed, which added about 1 ([jeena.net](https://jeena.net/lemmy-switch-to-piefed)).

**Lesson for us:** most of the performance comes from data access and caching design, not from the language.

### 3.2 Mbin / PieFed

**Mbin** is PHP/Symfony, AGPL-3.0. A bare-metal install needs PHP 8.3, Node 22, Redis/Valkey, Postgres, RabbitMQ, AMQProxy, Supervisor and nginx: a heavy service list ([Mbin install docs](https://docs.joinmbin.org/admin/installation/bare_metal/)).

**PieFed** is Python/Flask, with few dependencies, and is praised for low load ([Lemmy vs Mbin vs PieFed](https://lemmy.world/post/25339515)).

These are Reddit-style (votes, nested comments, communities), not classic forums.

### 3.3 Lobsters (key reference)

**Stack.** Rails, BSD-3. Moved MariaDB → **SQLite** in 2026, everything on one VPS:
- 3.8 GB content DB, plus SQLite cache, queue and rack-attack DBs.
- CPU and memory went down, and costs were roughly halved ([Simon Willison](https://simonwillison.net/2026/Jul/14/lobsters-sqlite/), [PR #1927](https://github.com/lobsters/lobsters/pull/1927)).

**Philosophy.** No JS for logged-out visitors, no JS build chain, almost no external services ([README](https://github.com/lobsters/lobsters)).

**How the logged-out view is served.** Read from the source:
- `ApplicationController::CACHE_PAGE` applies when there is no user, no tag-filter cookie and the session cookie can be cleared.
- The session cookie is actively **cleared for logged-out users so Caddy can serve the cache**.
- Pages are written as static `.html` files under `public/cache`, and **Caddy serves those files directly**, without touching Rails, when the request has no cookie.
- `CachePageJob` regenerates a story page in the background after edits, votes or comments, with a 2-minute delay.
- A cron job (`script/expire_page_cache`) deletes non-story cached files older than 3 minutes.
- Sources: [application_controller.rb](https://github.com/lobsters/lobsters/blob/main/app/controllers/application_controller.rb), [cache_page_job.rb](https://github.com/lobsters/lobsters/blob/main/app/jobs/cache_page_job.rb), [expire_page_cache](https://github.com/lobsters/lobsters/blob/main/script/expire_page_cache).

**Limitations.** No private sections. Invite-tree registration. Not packaged for general Docker deployment.

**Bot pressure.** Even so, lobste.rs search now sits behind an **Anubis proof-of-work challenge**. Fetching it during this research returned the Anubis page.

### 3.4 Tildes

Python 3 + Pyramid, PostgreSQL, Redis, intercooler.js + jQuery. "Boring" technology on purpose. AGPL-3.0.

"Tildes will always be functional to browse without javascript enabled" ([site implementation](https://docs.tildes.net/philosophy/site-implementation), [repo](https://gitlab.com/tildes/tildes)). Not really packaged for others to run.

### 3.5 Hacker News (UI and operations reference)

- A single application on closed-source Arc. It moved in Sept 2024 from Racket to **Clarc on SBCL**, which made it fast enough to **drop pagination on long threads** ([Lisp Journey](https://lisp-journey.gitlab.io/blog/hacker-news-now-runs-on-top-of-common-lisp/)).
- Storage was historically flat files rather than a database ([vale.rocks](https://vale.rocks/posts/hacker-news)).
- Logged-out users get a cached snapshot. During traffic spikes the moderators ask people to log out "so we can serve you from cache" ([HN item](https://news.ycombinator.com/item?id=6601327)).
- UI traits worth copying: dense table layout, tiny pages, near-zero JS (votes and collapse only), visible permalinks, and native browser find and scroll.

---

## 4. Niche and newer projects (2022–2026)

Found through GitHub search and HN Algolia.

**cerca** (Go, SQLite, AGPL-3.0).
- "Lean forum software" designed to minimise hosting and maintenance by leaving features out on purpose.
- **Threads are public by default and can be marked private (logged-in only).** This is the closest match to our public/private requirement.
- Also: invite system (single- and multi-use), public moderation log, two-admin approval for impactful actions, RSS, plain CSS.
- Source: [repo](https://github.com/cblgh/cerca).

**freedit** (Rust, MIT).
- Single binary with the embedded **fjall** KV store, Bulma CSS, fully usable without JS.
- "Inns" (sub-forums, can be private), "Solo" microblog, E2E-encrypted DMs, RSS reader.
- Shows a Rust forum can be one binary with zero external services. Source: [repo](https://github.com/freedit-org/freedit), [demo](https://freedit.eu/).

**Storyden** (Go + Next.js 16/React 19).
- A single `docker run`. SQLite by default, with Postgres or Turso optional, and migrations run automatically on start.
- RBAC roles are additive; the Guest role has `READ_PUBLISHED_THREADS`, among others.
- Scope is broad: forum + wiki "Library" + links + AI/MCP features.
- Licence: MPL-2.0 on GitHub, though PRODUCT.md says MIT.
- Sources: [repo](https://github.com/Southclaws/storyden), [database docs](https://www.storyden.org/docs/reference/database), [roles](https://www.storyden.org/docs/introduction/members/roles).

**Waterhole** (Laravel + Hotwire Turbo/Stimulus, by the Flarum creator).
- Server-rendered and works with JS disabled.
- The project claims 1.0 s LCP against 11.1 s for Discourse and 5.7 s for Flarum (its own benchmark).
- Private channels by group.
- Commercial: $299 per site, perpetual. Sources: [site](https://waterhole.dev/), [licensing](https://waterhole.dev/docs/main/licensing).

**Fider** (Go + React, AGPL-3.0). A feedback board, not a forum, but its rendering design is a **directly relevant pattern for a Rust + React stack**:
- Normal browsers get the SPA shell.
- When `IsCrawler()` matches, the server **renders the same React tree in embedded V8** (a `v8go` isolate pool) and serves `ssr.html`, with no JS bundle.
- An e2e test asserts that Googlebot gets SSR HTML and Chrome gets the SPA.
- Sources: [react.go](https://github.com/getfider/fider/blob/main/app/pkg/web/react.go), [renderer.go](https://github.com/getfider/fider/blob/main/app/pkg/web/renderer.go), [ssr.feature](https://github.com/getfider/fider/blob/main/e2e/features/server/ssr.feature).
- The equivalent in Rust would be `deno_core`/`rusty_v8`.

**Apache Answer** (Go + React, Apache-2.0). A Q&A site that runs as a **single container** with an `answer-data` volume ([repo](https://github.com/apache/answer)). Good packaging reference.

**Talkyard** (Scala/Play + React, AGPL-3.0). Has private categories and anonymous posting, but needs nginx, app (JVM), Postgres, Redis and ElasticSearch, with memory profiles from 2 GB up ([hosting requirements](https://forum.talkyard.io/-100/hosting-requirements), [talkyard-prod-one](https://github.com/debiki/talkyard-prod-one)). An example of a stack that is heavy to self-host.

**agreper** (Flask + SQLite, no JS). 392 points on HN (2022). Commenters praised its speed: "one of the fastest forums I've used". Spam and moderation tooling were the worry. NodeBB's founder replied that "building a forum software isn't as easy as you think". Dormant since 2024 ([HN](https://news.ycombinator.com/item?id=33153152)).

**NimForum** (Nim, SQLite, Karax SPA). A "lightweight Discourse alternative", but it is still a JS SPA. HN commenters complained about payload size, including reCAPTCHA ([HN](https://news.ycombinator.com/item?id=28329672)).

**Others seen.**
- Gosora (Go; templates compiled to Go code; GPL-3; dormant since 2023) ([repo](https://github.com/Azareal/Gosora)).
- orangeforum (Go; BSD-3; dormant 2024).
- GooseForum (Go; MIT; active).
- FlaskBB (Python; BSD; active).
- Squalk (SvelteKit on Nostr, "an old school forum on an open protocol", Sept 2026) ([HN](https://news.ycombinator.com/item?id=49909980)).
- Nodyx (Svelte; forum + chat + voice; AGPL).
- CForum (Cloudflare Workers + D1).
- mitsuhiko/minibb (vibe-coded demo).

**Discord-specific tool.** [Answer Overflow](https://news.ycombinator.com/item?id=36383773) (open source) exists only to make selected Discord channels publicly indexable. This shows real demand for "the Discord community's public knowledge on the open web".

---

## 5. Cross-cutting findings

### 5.1 How anonymous and crawler traffic is served

| Approach | Used by | Notes |
|---|---|---|
| Classic server-rendered HTML for everyone | phpBB, MyBB, SMF, XenForo, cerca, freedit, Tildes, Waterhole | Simple and fast first paint. Every request still hits the app and DB unless cached. |
| Server-rendered + **static file cache served by the web server** for cookie-less requests | **Lobsters**, HN (in-app cache) | Cheapest per anonymous request. Needs invalidation logic and cookie discipline. |
| SPA + **separate crawler-only view** chosen by User-Agent | **Discourse** (ERB template), **Fider** (React rendered in V8) | Crawlers get HTML. Humans without JS get little or nothing. Two render paths to keep in sync (Fider avoids this by reusing the same React tree). |
| SPA with server-rendered first load, client router afterwards | NodeBB, Lemmy-ui, Talkyard, Storyden (Next.js) | Needs a JS runtime on the server (Node or Next), adding a service and memory. |
| App-level anonymous cache (Redis) | Discourse | Short TTL (60 s). Has had cache-poisoning CVEs. |

### 5.2 AI scrapers are now a first-order design constraint

- In 2025 phpBB admins reported 13,000 concurrent "users" instead of the usual 100–250, mostly scrapers. Forums went down for days. Fixes included nginx `limit_req` on `.php` and the AnubisBB proof-of-work extension ([The Register](https://www.theregister.com/2025/07/09/anubis_fighting_the_llm_hordes/), [AnubisBB](https://www.phpbb.com/community/viewtopic.php?t=2662765)).
- Lobsters itself now gates search behind Anubis.
- Serving anonymous pages as pre-rendered static bytes keeps the cost of this traffic low. Rate-limiting expensive uncached endpoints (search, deep pagination, user listings) is equally important.

### 5.3 Install and maintenance burden (for "easy docker compose")

- **Lowest:** single binary or single container with SQLite or an embedded store: freedit, cerca, Apache Answer, Storyden. Lobsters on SQLite shows a large real site can run like this.
- **Medium:** app + Postgres (Fider, NodeBB + Mongo/PG), or LAMP boards without official images (phpBB, Flarum, MyBB).
- **High:** Discourse (custom launcher, rebuilds, 2 GB+), Lemmy (5 services), Talkyard (JVM + ElasticSearch), Mbin (RabbitMQ and more).
- **Upgrade pain** comes mostly from plugin and extension ecosystems (Discourse, NodeBB, Flarum, phpBB). A deliberately small feature set with **no plugin API at first** sidesteps it.

### 5.4 Licences

| Licence | Projects |
|---|---|
| Permissive (MIT / BSD / Apache) | Flarum, Lobsters, SMF, freedit, NimForum, Apache Answer, FlaskBB |
| Weak copyleft (LGPL / MPL) | MyBB (LGPL-3), Storyden (MPL-2.0) |
| GPL | Discourse, phpBB, Misago, Vanilla (GPL-2); NodeBB, Gosora (GPL-3) |
| AGPL | Lemmy, Mbin, PieFed, Tildes, Fider, Talkyard, cerca, agreper |
| Proprietary / source-available | XenForo, Waterhole |

AGPL is the norm among newer community-run projects. MIT/BSD dominates the projects that are "infrastructure-like".

### 5.5 Fit for the first community (500-person Discord → forum)

| Need | Who does it well | Gaps |
|---|---|---|
| **Flat threads** | phpBB, SMF, XenForo, Discourse (flat with reply links) | Lobsters, HN, Lemmy and freedit are nested or tree-style. |
| **Members-only categories gated by group** | Discourse (See/Reply/Create), NodeBB, phpBB, XenForo | Simplest correct model to copy: Discourse's per-category group list with 3 levels. Avoid phpBB's Yes/No/Never complexity. cerca has per-thread private only, without groups. |
| **DMs** | Discourse, NodeBB, phpBB, XenForo | Flarum's DMs are still beta. |
| **Discord login** | Discourse (core) | Helps account linking from Discord. |
| **Discord announcement bridge** | Discourse (chat integration) | |
| **Discord history import** | Nobody (Discourse explicitly doesn't support it) | |

---

## 6. Gaps in the landscape

1. **Nothing is lightweight, modern-looking, easy to deploy and has proper group-gated private categories all at once.**
   - The lightweight projects (cerca, freedit, agreper, NimForum) are tiny, single-maintainer and limited. cerca has only a public/private flag per thread; agreper has no permissions.
   - The projects with good permissions and DMs (Discourse, NodeBB, phpBB, XenForo) are heavy, dated or proprietary.
2. **Almost nobody combines a modern JS frontend with zero-JS, cache-served pages for anonymous users.**
   - Discourse and Fider serve HTML only to crawlers chosen by User-Agent, so humans without JS get nothing.
   - The no-JS projects ship weak logged-in UX.
   - Lobsters' file cache is the cleanest design for anonymous traffic, but it is Rails, a link aggregator and not packaged for others.
3. **Simple, auditable permission models are missing.** The options are phpBB's ACL maze or "public or logged-in" binaries. A Discourse-style per-category group list (see / reply / create), plus per-thread overrides, is about the right size, and nobody minimal ships it.
4. **One-container deploys with SQLite are rare among real forums.** Exceptions: Storyden (but heavy scope and Next.js), freedit (embedded KV), Answer (Q&A). Lobsters proves SQLite holds up at real scale.
5. **Bot and scraper resilience** is bolted on (Anubis, nginx rate limits) rather than designed in.
6. **No migration path from Discord**, though Answer Overflow shows demand for making Discord content public.

---

## 7. Most useful references for a minimal, fast design

1. **Lobsters.** The model for serving anonymous traffic:
   - Strip cookies from anonymous requests.
   - Write rendered pages to disk and let the reverse proxy serve them.
   - Re-render in the background when content changes, with a time-based sweep for listing pages.
   - Run on SQLite on one VPS.
   - Send no JS to logged-out users.
2. **Fider.** The model for using **one React component tree for both the SPA and the server HTML**: render React in an embedded V8 isolate pool from a compiled language. In Rust this maps to `deno_core`/`rusty_v8`. Alternatively, render server HTML natively in Rust and hydrate React only for logged-in users. Fider's use of User-Agent only for crawlers is the part *not* to copy: serve HTML to all anonymous users.
3. **Discourse's category security model** (group × {See, Reply, Create}) and its anonymous cache idea. Avoid its SPA-only human UX, infinite scroll and heavy deploy.
4. **cerca.** For deliberately limited scope, invites, public moderation log, per-thread private flag, single binary + SQLite, and AGPL.
5. **freedit.** Proof that a Rust forum can be one binary with an embedded store and no JS requirement.
6. **Hacker News.** For UI density and page weight, native scrolling and find, and full threads without pagination.
7. **Waterhole and Misago.** Both moved away from client-side frameworks: Flarum's creator went to Hotwire, and Misago is removing React. Use React for logged-in interactivity only, not as the delivery vehicle for content.
8. **Lemmy.** A warning: a Rust backend alone did not make it fast. Query design, indexes and caching did.
9. **Apache Answer and Storyden.** For packaging: a single container, SQLite default with Postgres optional, migrations run on start.
