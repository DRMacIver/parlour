# forumal — design (draft v0.1)

A minimal, fast, self-hosted web forum in the old-school style. MIT licensed. The background research is in `research/`; start with `research/00-summary.md`.

Items marked **[proposal]** are choices I made while drafting this document that we haven't discussed yet.

## 1. Principles

1. **Pages are HTML.** Every page is rendered on the server and works without JavaScript. JS adds to a page; nothing depends on it.
2. **Anonymous visitors cost almost nothing.** A logged-out visitor's request is served from an in-memory cache, with no database query and no cookies set.
3. **One permission check, used everywhere.** All content is read through one access layer that takes a `Viewer`. Search, notifications, emails, embeds, feeds and the anonymous cache all go through it.
4. **Small deliberate scope.** There is no plugin API. Each feature has to earn its place.
5. **One box.** One app process, one Postgres, behind Caddy. There is no horizontal scaling, and the design depends on that: for example, cache invalidation happens inside the one process.

## 2. Scope

### v1

- Categories with three visibility levels: public, members-only, and restricted to groups.
- Flat, chronological, paginated threads, with a stable permalink for every post.
- Markdown posts with a preview; editing and deleting your own posts.
- Replying with a quote, which records an "in reply to" link to the quoted post.
- **Quote-forks:** starting a new thread that embeds an existing post, like a quote tweet, with a backlink on the original post.
- Reactions from a small set of emoji chosen by the admin.
- A per-thread read marker, "jump to first unread", and unread indicators in thread lists.
- Watching threads and categories; in-app notifications for replies, mentions, DMs and watched threads; email notifications only when the user is away.
- DMs between users, implemented as private threads with a list of participants.
- Invites as a first-class concept, a public tree of who invited whom, and invite-only registration by default.
- Accounts: email with a password or a magic link, email verification, password reset.
- Groups, managed by admins and assignable through invites.
- Moderation:
  - a report button and one review queue;
  - hide, delete, lock, pin and move threads, and move posts between threads (which covers split and merge);
  - timed silence and suspend;
  - a moderation log (the entries about public content are visible to everyone) and staff-only notes on users.
- Permission-aware full-text search using Postgres.
- Image uploads in posts and avatars, with metadata removed and size limits **[proposal: in v1 because Discord users expect to share images]**.
- User profiles showing avatar, title and who invited them.
- **No signatures.** Instead, each user can customise the post-info column shown to the left of their posts:
  - a title;
  - pronouns;
  - a short status line of up to about 60 characters, plain text with no links;
  - an optional location.

  The column always shows the avatar, display name, username and join date. The custom fields are short and fixed in format, so the page layout stays dense and predictable. **[proposal: exact fields]**
- **Edit history.** Editing has no time limit. An edited post shows "edited", which links to its revisions, and anyone who can see the post can see its revisions **[proposal]**.
- Data export and account deletion; a simple contact and report form that works without logging in.
- Live "N new posts" indicator on threads using server-sent events **[proposal]**.
- Deployment: docker compose, a first-run `/setup` wizard, automatic migrations, nightly backups.

### Soon after

- Open registration modes (email-verified, admin approval), with the anti-bot measures they need: a hidden honeypot field, a minimum form-fill time and a proof-of-work check **[proposal: invite-only means these can wait]**.
- Passkeys and generic single sign-on (OIDC); Discord as an optional single sign-on provider.
- Digest emails, Atom feeds, a webhook to post new threads elsewhere (for example into Discord), bookmarks, web push notifications, templates for terms of service, privacy and Online Safety Act paperwork, importers, spoiler markup.

### Out of scope

- ActivityPub, end-to-end encrypted DMs, real-time chat.
- Gamification and trust-level ladders. The only exception is a simple "new user" limit.
- Native apps, reply-by-email, a plugin API.
- Multiple communities in one install. Run a separate instance per community instead.

## 3. Domain model

### 3.1 Visibility

Each category has a **read level**:

- `public`: anyone, including anonymous visitors.
- `members`: any logged-in user who is not suspended.
- `groups`: members of one or more listed groups.

Each category also has a **write level**: `members`, `groups` (one or more listed groups), or `staff`.

Admins and moderators can see everything **[proposal: per-category moderators can come later]**.

Each thread stores a copy of its visibility, `threads.visibility`, which is one of `public`, `members`, `groups` or `private`. The copy is updated whenever the thread moves or its category's settings change. That lets the anonymous path filter with a single `WHERE visibility = 'public'`. `private` is used for DMs.

**The "at most as visible" rule.** Destination D is at most as visible as source S if everyone who can see D can also see S. It's computed conservatively:

| S \ D | public | members | groups(Gd) | private |
|---|---|---|---|---|
| public | ✓ | ✓ | ✓ | ✗ |
| members | ✗ | ✓ | ✓ | ✗ |
| groups(Gs) | ✗ | ✗ | ✓ if Gd ⊆ Gs | ✗ |
| private | ✗ | ✗ | ✗ | ✗ |

Content in DMs can never be quoted out, and DMs never contain quote-forks **[proposal]**.

### 3.2 Quote-forks

- A thread can have `forked_from_post_id`. Its page shows the source post embedded above the first post, the way a quote tweet shows the original.
- **Creating a fork is refused** unless the destination category is at most as visible as the source post's thread.
- **Embeds are references, not copies.** The embedded post is checked against the viewer's permissions every time the page is rendered. If the source is later moved somewhere more private, or deleted, the embed shows "this post is unavailable". That also covers moves after the fork was created, which the rule above can't prevent.
- The source post shows "↪ continued in: <title>" backlinks. Each one is shown only to viewers who can see the fork.
- Editing the source post updates every embed. For the cache, see §5.
- An embed always shows the current version of the source post, marked "edited" if it has been. That keeps embeds consistent with the edit history and the "references, not copies" rule. **[proposal]**

### 3.3 Threads and posts

- Each post has `seq`, its number within the thread, starting at 1 and never reused. A deleted post becomes a tombstone ("post deleted"), so page boundaries never shift.
- Pages hold 50 posts **[proposal]**. A post's page is `ceil(seq / 50)`.
- A post can have `reply_to_post_id`, which must be in the same thread. Selecting text and clicking Quote inserts a Markdown blockquote and sets this link. The post is then shown with "in reply to @alice ↑", linking to the quoted post.
- Markdown follows CommonMark plus GitHub extensions (tables, strikethrough, autolinks), rendered by comrak and sanitised by ammonia.
- **HTML is generated when a post is saved** and stored as `body_html`. Mentions (`@name`) are resolved to links at that point.
- **Edits:**
  - Posts can be edited at any time.
  - Each edit stores the previous version in `post_revisions`.
  - Edits within 5 minutes of posting are merged into one revision, as a grace period for typo fixes **[proposal]**.
  - `/p/{id}/revisions` shows each revision with a diff.
- **Redacting revisions:** a moderator can redact an individual revision, for example if someone edited out doxxing and the old text is still in the history. A redacted revision keeps its entry but its content is removed for everyone, and the action is recorded in `mod_actions`.
- **Deleting a post** also hides its history.

### 3.4 Reactions

- The admin configures a set of emoji (up to about 8; defaults something like 👍 ❤️ 😂 😮 😢 🙏).
- A user may add several different reactions to a post, but each one only once.
- Posts show counts. Hovering, or a tooltip, shows who reacted.
- Reactions have no other effect: they don't change sort order, don't bump the thread and don't send notifications by default **[proposal]**.
- Without JS, reactions are submitted as small forms.

### 3.5 Read tracking

`thread_reads(user_id, thread_id, last_read_seq)`.

- The marker only moves forward: it's set to the highest post `seq` that has been displayed to the user.
- "Jump to first unread" goes to post `last_read_seq + 1`.
- Thread lists show unread counts as `last_seq - last_read_seq`.
- A whole category can be marked as read.

### 3.6 Invites

Invites are first-class and form a tree.

- `invites(id, code_hash, created_by, email?, max_uses, uses, expires_at, grant_group_ids[], note, created_at, revoked_at)`
- `users.invited_by_user_id` and `users.invite_id` are recorded on sign-up and can't be changed.
- **Who can invite** is a site setting: everyone, members older than N days, or staff only. Inviting someone sends them an email.
- Staff can also create links with several uses and groups attached, for onboarding a whole community at once.
- **The tree is public:**
  - `/users/tree` shows the full tree;
  - each profile shows "invited by X" and the people that user invited.
- Moderators see the tree in the user admin view. That makes it easy to spot bad branches, and "suspend this user and everyone they invited" can be a single action **[proposal]**.

### 3.7 DMs

- A DM is a thread with `visibility = 'private'`, no category, and a participant list in `private_thread_participants(thread_id, user_id, joined_at, left_at)`. DMs therefore get pages, unread markers, reactions and notifications for free.
- They never enter the anonymous cache, search for other users, feeds, digests or the public moderation log.
- Search shows DMs only to their participants **[proposal: later]**.
- Safety controls:
  - a site-wide on/off switch;
  - a minimum account age or post count before a user can start DMs;
  - a cap on recipients and a rate limit;
  - a "who can DM me" setting: everyone, people I've interacted with, or nobody;
  - blocking users, and reporting a DM post. Reporting reveals that conversation to moderators; other DMs stay hidden.
- Messages from staff can't be blocked.

## 4. Data model (Postgres)

IDs are `bigint` identity columns. Times are `timestamptz`.

```
users(id, username UNIQUE citext, display_name, email UNIQUE citext, email_verified_at,
      password_hash?, role {user, moderator, admin}, title?, pronouns?, status_line?, location?, avatar_upload_id?,
      invited_by_user_id?, invite_id?, silenced_until?, suspended_until?,
      dm_policy, email_prefs jsonb, created_at, deleted_at?)
groups(id, slug, name, description)
group_members(group_id, user_id, added_by, added_at)
categories(id, slug, name, description, position, read_level, write_level, version)
category_groups(category_id, group_id, access {read, write})
threads(id, category_id?, visibility, title, slug, author_id, forked_from_post_id?,
        last_seq, last_post_at, pinned, locked, deleted_at?, version, created_at)
private_thread_participants(thread_id, user_id, joined_at, left_at?)
posts(id, thread_id, seq, author_id, reply_to_post_id?, body_md, body_html,
      search tsvector GENERATED, created_at, edited_at?, deleted_at?, hidden_at?,
      UNIQUE(thread_id, seq))
post_revisions(id, post_id, rev, body_md, body_html, editor_id, created_at, redacted_at?, redacted_by?)
reaction_types(emoji, position, enabled)
reactions(post_id, user_id, emoji, created_at, PK(post_id, user_id, emoji))
thread_reads(user_id, thread_id, last_read_seq)
watches(user_id, thread_id? | category_id?, level {watching, normal, muted})
notifications(id, user_id, kind, actor_id?, thread_id?, post_id?, created_at, read_at?, emailed_at?)
invites(...)                                  -- §3.6
sessions(token_hash, user_id, created_at, last_seen_at, ip, user_agent)
login_tokens(token_hash, user_id, purpose {magic_link, verify_email, reset}, expires_at, used_at?)
blocks(user_id, blocked_user_id)
reports(id, reporter_id?, contact_email?, target_kind, target_id, reason, status,
        resolved_by?, resolution?, created_at)
mod_actions(id, actor_id, action, target_kind, target_id, reason, public, details jsonb, created_at)
user_notes(id, user_id, author_id, body, created_at)
uploads(id, user_id, sha256, mime, bytes, width, height, created_at)
jobs(id, kind, payload jsonb, run_at, attempts, locked_at?, last_error?)
settings(key, value jsonb)
```

Using a single `threads` table for both categories and DMs is a deliberate choice. Research report 03 suggested separate tables for DMs. I went the other way because report 04 recommended DMs as private threads, and because the `visibility` column, together with the access layer, enforces the separation. **[proposal; worth discussing]**

## 5. Rendering and caching

- **Templates:** askama, compiled into the binary.
- **CSS:** one hand-written stylesheet using CSS variables, with light and dark themes. The look is dense and in the style of phpBB/old.reddit, and adapts down to 320px wide.
- **React "islands":**
  - Built with Vite. Each island mounts on an element with `data-island="composer"` and reads its props from a `<script type="application/json">` tag next to it.
  - v1 islands: the composer (Markdown with server-rendered preview, quote insertion, image paste), the reaction picker, the notifications menu, the live "new posts" bar, and the quote-fork dialog.
  - Each island replaces a working plain-HTML form.
  - Anonymous pages load no island JS.

### Anonymous cache

- **When it applies:** a `GET` request with no session cookie is served from an in-memory cache keyed by path and query. On a miss, the page is rendered with `Viewer::Anonymous` and then stored.
- **Never set cookies for anonymous visitors.** The login and invite forms protect against cross-site request forgery with SameSite cookies and an `Origin` check, not CSRF tokens, so even those pages can be cached.
- **Invalidation:** each cache entry is tagged with what it depends on: `thread:N`, `category:N` and `site`. After each write commits, the app clears the matching tags. This happens in-process, because there is exactly one process.
  - Editing post P clears the tag of P's thread, plus every thread with `forked_from_post_id = P`.
  - Renaming a user or changing site settings clears `site`.
- **Headers:**
  - Anonymous responses: `Cache-Control: public, max-age=0, s-maxage=30, stale-while-revalidate=60`, plus an `ETag` built from the version counters, so a CDN in front works.
  - Everything else: `private, no-store`.
  - We never rely on `Vary: Cookie`.
- **Logged-in pages are rendered fresh** on every request. They're still cheap: a few indexed queries and a compiled template.
- **Live updates:** the thread page opens a server-sent events stream for that thread, after a permission check. New posts are broadcast in-process.

## 6. Access layer

```rust
enum Viewer { Anonymous, User(UserCtx) }  // UserCtx: id, role, group_ids, suspended/silenced
```

- Every content query lives in one `content` module, and every function in it takes `&Viewer`. The visibility condition is a single shared SQL fragment.
- Handlers can't reach the tables any other way: they only get the `content` API, not raw pool access.
- Search, notifications, email rendering, embeds and backlinks all call the same functions.

**Property-based tests (Hegel)** check the access layer. Given randomly generated categories, groups, users, threads, forks, moves and deletions:

- every item returned to a viewer is one that the reference permission function, written separately, allows;
- the anonymous-rendered HTML of every page contains no text from posts that aren't public.

## 7. Accounts, auth, anti-abuse

- Passwords are hashed with Argon2id and are optional; users can sign in by magic link instead.
- Sessions are opaque random tokens, stored hashed in the `sessions` table, in a `HttpOnly; Secure; SameSite=Lax` cookie.
- **Registration modes in v1:** `invite_only` (the default) and `closed`.
- **Rate limits:** per IP and per account on login, magic-link requests, posting, DMs, reports and search. They're held in memory as token buckets.
- **New-user limits** apply until an account is 3 days old or has 5 posts, whichever comes later (both thresholds configurable). New users get lower posting and DM rates and can't start DMs. **[proposal]**

## 8. Moderation

- **Report** is available on posts, DMs and profiles. There is also a contact and report form for people who aren't logged in.
- **`/mod/queue`** lists open reports, oldest first, with one-click actions.
- **Actions:**
  - hide post, which keeps it for staff and shows a tombstone to everyone else;
  - delete post;
  - lock, pin, move or rename a thread;
  - move posts to another or a new thread (which covers split and merge);
  - silence or suspend for a duration with a reason;
  - delete as spammer, which removes the account and its content;
  - suspend an invite subtree.
- **Every action is recorded in `mod_actions`.** `/moderation` shows the entries about public content to everyone, in the style of Lobsters.
- **Staff-only notes** on users.

## 9. Search, notifications, email, jobs

- **Search:** a generated `tsvector` on posts and threads, queried with `websearch_to_tsquery`. The visibility filter is part of the same SQL query. Search is rate-limited.
- **Notifications** are rows in the `notifications` table. A badge in the page header shows unread notifications; the island updates it over server-sent events.
- **Email:**
  - Sent through any SMTP relay using lettre, via a job queue in Postgres (`SKIP LOCKED`) with retries.
  - A notification is emailed only if it's still unread after N minutes and the user hasn't been seen during that time.
  - Emails carry one-click unsubscribe headers. The admin can send a test email.
- **Uploads:**
  - Images are stored on a volume under their content hash.
  - Each image is re-encoded to remove EXIF metadata, with limits on file size and dimensions.
  - Images are served by the app with long cache lifetimes. Images attached to private content are checked against permissions **[proposal: simpler alternative is unguessable URLs]**.

## 10. Deployment

- **The image:** one static binary with the templates and Vite output embedded, built from a multi-stage Dockerfile into a small final image.
- **`compose.yaml`** runs `forumal`, `postgres:18` (pinned major version), `caddy` (automatic HTTPS) and `backup` (a nightly `pg_dump` plus the uploads volume, with rotation).
- **Configuration** comes from environment variables: `FORUMAL_DOMAIN`, `POSTGRES_PASSWORD`, and the SMTP settings. Everything else is set in the admin interface.
- **First run:** the app prints a one-time setup token. `/setup?token=…` creates the admin account, the site name and the SMTP settings (with a test send), then issues the first invites.
- **Migrations** use sqlx and run on startup, after a pre-migration dump. The app refuses to start if it detects an empty database where it expected existing data, which guards against the postgres:18 data-directory change.
- **Docs:** how to upgrade the Postgres major version, and how to restore a backup.

## 11. URLs

| Path | Page |
|---|---|
| `/` | Category index (phpBB style), plus latest activity |
| `/latest` | All threads by last activity |
| `/c/{slug}?page=n` | Category: pinned threads first, then by last activity |
| `/t/{id}/{slug}?page=n` | Thread |
| `/t/{id}/unread` | Redirects to the first unread post |
| `/p/{id}` | Post permalink; redirects to `/t/…?page=k#p{id}` |
| `/p/{id}/revisions` | Edit history |
| `/u/{username}` | Profile |
| `/users/tree` | Invite tree |
| `/search?q=` | Search |
| `/messages`, `/messages/{id}` | DMs |
| `/notifications`, `/settings`, `/invites` | Personal pages |
| `/invite/{code}`, `/login`, `/login/{token}`, `/logout` | Auth |
| `/moderation` | Public moderation log |
| `/mod/…`, `/admin/…`, `/setup` | Staff pages |
| `/api/…` | JSON endpoints for islands (the same access layer) |
| `/events/t/{id}` | Server-sent events stream |

## 12. Repository layout

```
server/        Rust crate (axum, sqlx, askama); migrations in server/migrations
web/           Vite + React + TypeScript islands; built output embedded into server
deploy/        compose.yaml, Caddyfile, .env.example, backup script
docs/          admin guide (install, upgrade, backup/restore)
research/      background research
```

**Testing:**

- Rust integration tests run against real Postgres (`#[sqlx::test]` gives each test its own database).
- Hegel property tests cover the access layer, the cache and pagination.
- Playwright tests cover the main flows, with and without JS.
- CI runs all of them.

## 13. Milestones

1. **Skeleton:** axum, sqlx, migrations, a base template, compose, CI.
2. **Reading:** categories, threads and posts with pagination and permalinks; the anonymous cache; seed data.
3. **Accounts:** sessions, login, invites and the invite tree, the setup wizard.
4. **Writing:** the composer, Markdown, editing, reply-to, reactions, read markers, quote-forks.
5. **Privacy:** groups, members-only and group-restricted categories, the access-layer property tests.
6. **DMs and notifications:** DMs, notifications, watching, email, live updates.
7. **Moderation:** reports, the queue, actions, the logs, limits on new users.
8. **Finishing v1:** search, uploads, data export and deletion, backups, admin documentation.

## 14. Open questions

1. Which **[proposal]** items do you want to change?
2. **Category layout:** a flat list, a flat list with optional display-only sections, or real subforums? (Recommendation: a flat list with optional sections.)
3. **Name:** "forumal" is a placeholder. Choose a real name before the first public release.

Resolved: no signatures, but a customisable post-info column; no time limit on editing; edit history in v1.
