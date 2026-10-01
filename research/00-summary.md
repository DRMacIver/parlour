# 00 — Research summary and proposed direction

This summary combines four research reports:

- [01-landscape.md](01-landscape.md): existing forum software
- [02-ux-sentiment.md](02-ux-sentiment.md): what people like and dislike about forum interfaces
- [03-architecture.md](03-architecture.md): rendering, caching, permissions, stack, deployment
- [04-essentials.md](04-essentials.md): spam, moderation, accounts, DMs, email, UK Online Safety Act

Decisions already made: Rust backend, React where it is useful, Postgres, open source, docker compose deployment on a single small VPS. Threads are flat and chronological, and any post can be quoted into a new thread. Categories can be public, members-only or restricted to groups. DMs are in scope. Discord login is optional.

## The gap

No existing project is lightweight, modern, easy to deploy and also has members-only categories restricted by group, flat threads and DMs.

- Discourse has all of these features, but it is heavy: Rails + Ember, about 2 GB of RAM, a painful rebuild step on upgrade, nothing usable without JavaScript, and infinite scroll.
- The well-known PHP forums (phpBB, MyBB, SMF) are dated. Flarum is stuck in 2.0 release candidates, and NodeBB plugins break on upgrade.
- The newer minimal projects (cerca, freedit) are small single-maintainer efforts with only basic permissions.

Server-rendered HTML is coming back: Flarum's creator built Waterhole without a JS framework, and Misago is removing React.

## What users want (from 02)

- **Flat threads** suit small communities that check in daily. Most forums that have lasted ten years or more are flat. Store a "reply to" link on posts to deal with quotes from far back in a thread.
- **Real pages and a permalink for every post.** Infinite scroll is the most-disliked Discourse feature.
- **Unread tracking per thread, with "jump to first unread".** Everyone likes it.
- **Dense, fast pages that work without JS**, in the style of old.reddit and phpBB, and good on phones.
- **Avatars and user titles** help people recognise each other. Signatures should be light or collapsed.
- **No gamification**: no badges, karma or visible scores. Reactions are tolerated because they reduce "+1" posts.
- **Quoting a post into a new thread** has prior art in Discourse's "reply as linked topic", but half of users never find it. Put it next to Quote and Reply, and link back from the original thread.
- **Moving a Discord community over:** the forum should sit alongside Discord, the most active members need to come first, and Discord login plus a bot that posts new threads into Discord help.

## Architecture (from 03)

- **Rendering:** every page is HTML rendered by the Rust server (askama templates). Logged-out visitors get essentially no JS. Logged-in users get React components for interactive parts: the composer, notifications, live updates and reactions. Rendering React on the server from Rust is not recommended. **Needs a decision, see below.**
- **Caching for logged-out visitors:**
  - Pages are cached in memory only for requests with no session cookie, and are only ever rendered as an anonymous viewer.
  - Each thread and category has a version counter, increased in the same transaction as the write. These counters drive cache invalidation (via Postgres LISTEN/NOTIFY) and ETags.
  - Responses use `public, s-maxage, stale-while-revalidate` for anonymous pages and `private, no-store` for anything else.
  - Don't rely on `Vary: Cookie` (see Discourse CVE-2024-47773).
- **Permissions:**
  - Each category gives groups the right to see it, reply in it or start threads in it.
  - The visibility tier is copied onto each thread.
  - Every read goes through one access layer that requires a viewer, so a forgotten permission check fails to compile.
  - DMs are stored separately and never go into the shared cache, search, feeds or digests.
- **Stack:** axum, sqlx, Postgres 18.
  - Postgres full-text search with the permission filter in the same query.
  - LISTEN/NOTIFY with server-sent events for live updates.
  - A job queue in Postgres using `SKIP LOCKED`.
  - Opaque database sessions, comrak and ammonia for Markdown, lettre for SMTP.
- **Deployment:**
  - One small image containing a single binary with embedded assets.
  - The compose file runs the app, Postgres, Caddy and a nightly `pg_dump` backup.
  - Required settings are the domain, the database password and the SMTP details.
  - First-run setup is a `/setup` wizard unlocked by a token printed in the logs.
  - Migrations run automatically, with a dump taken before each one.
  - The app guards against the postgres:18 data-directory change.
  - A second community runs as a separate instance.

## Essentials (from 04)

- **Spam:**
  - The cheap, effective core needs no third parties: choice of registration modes (default: email-verified plus admin approval, or invite-only), a honeypot field with a minimum fill time, a self-hosted proof-of-work challenge, limits on new users, approval of first posts, rate limits, and a one-click "delete as spammer".
  - AI scrapers are a separate threat to availability, which the anonymous page cache addresses.
- **Moderation:**
  - One review queue for reports, held posts and pending users.
  - Hide, delete, lock, pin and move, plus moving posts between threads (which covers split and merge).
  - Timed silence and suspend.
  - A moderation log and staff-only notes on users.
- **DMs:** implement them as private threads with a list of participants. Safety controls:
  - a site-wide on/off switch;
  - new users can't start DMs until they qualify;
  - "who can DM me" and block settings;
  - reporting DMs.
- **UK Online Safety Act:** the duties for small sites are mostly about process: a written risk assessment, a way to report and complain (including without logging in), terms of service, and records of decisions. Built-in templates and DM controls cover the reasons small UK forums gave for shutting down.

## Proposed v1 scope (my synthesis; to be discussed)

The essentials report's "MVP" list is large. To keep v1 minimal, I would trim it to:

**In v1**

- Categories (public, members-only, group-gated), flat paginated threads, Markdown posts with preview, editing.
- Quote-into-new-thread, with a backlink in the original thread.
- Per-thread unread tracking and jump to first unread.
- Watching threads and categories, in-app notifications for replies, mentions and DMs, and optional emails sent only when the user is away.
- DMs as private threads with participant lists, with the safety controls listed above.
- Accounts:
  - email with a magic link or password;
  - invite links that can automatically add people to groups;
  - optional Discord login, restriction to members of a Discord server, and syncing Discord roles to forum groups at login.
- Anti-spam: the self-contained core described above.
- Moderation: the review queue, the essential actions, the log and user notes.
- Permission-aware Postgres search.
- Anonymous page cache, docker compose, setup wizard, backups.
- Data export and account deletion.

**Soon after**

- Generic OIDC login and passkeys.
- Weekly digest emails.
- Atom feeds.
- A webhook that posts new threads into Discord.
- Reactions and bookmarks.
- Edit history visible to staff.
- Templates for terms of service, privacy and OSA paperwork.
- Web push notifications.
- Importers.

**Out of scope**

- ActivityPub, end-to-end encrypted DMs, real-time chat, gamification, native apps.
- Reply-by-email and running a mail server.
- A plugin API, at least initially, because most upgrade pain in other forums comes from plugins.

## Decisions needed

1. **Frontend shape.** Option A (recommended by 03): every page is rendered by the server, with React components for interactive parts. Option B: a full React app for logged-in users, with server-rendered HTML only for logged-out visitors. A is faster and keeps one set of markup. B is a "real" React app but means maintaining two renderings of every page.
2. **Licence.** AGPL (most newer community projects, and it keeps hosted forks open) or MIT/Apache (easier to adopt).
3. **Quoting from a private area into a less private one:** refuse outright, or refuse with a logged override for moderators. Should quotes be live references that are checked when shown, or copied text?
4. **Reactions in v1** or later.
5. **Default registration mode** for new installs: invite-only, or email-verified plus approval.
6. **Read tracking:** a per-thread "read up to here" marker (cheap and natural for flat threads) or tracking each post individually.
