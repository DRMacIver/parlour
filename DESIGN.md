# Parlour — design (draft v0.2)

A minimal, fast, self-hosted web forum in the old-school style. MIT licensed. The background research is in `research/`; start with `research/00-summary.md`, and `research/05-organisation-summary.md` for the organisation model introduced in v0.2.

Items marked **[proposal]** are choices I made while drafting this document that we haven't discussed yet.

**What changed from v0.1.** v0.1 had a fixed, admin-built list of categories under display-only section headings, groups managed by admins, staff who could see everything, and notifications for watched categories. v0.2 replaces the categories with **spaces** that any member can create, gives every member a **feed** (a space they own), lets members build their own **lists** and use them as permissions, lets each member arrange their own **front page** over an admin default, adds **per-thread permissions**, cuts notifications down to mentions and subscribed threads, and makes staff access to private content a logged, announced **break-glass** action. The prototype built against v0.1 is being migrated; §13 lists the steps. The decisions behind the changes, with the user research, are in `research/05-organisation-summary.md`.

## 1. Principles

1. **Pages are HTML.** Every page is rendered on the server and works without JavaScript. JS adds to a page; nothing depends on it.
2. **Anonymous visitors cost almost nothing.** A logged-out visitor's request is served from an in-memory cache, with no database query and no cookies set.
3. **One permission check, used everywhere.** All content is read through one access layer that takes a `Viewer`. Search, notifications, emails, embeds, Atom feeds and the anonymous cache all go through it. Staff roles grant powers, not readership: a moderator reads what a member with the same memberships could read, and anything beyond that is a break-glass access (§8).
4. **Small deliberate scope.** There is no plugin API. Each feature has to earn its place.
5. **One box.** One app process, one Postgres, behind Caddy. There is no horizontal scaling, and the design depends on that: for example, cache invalidation happens inside the one process.
6. **Nothing is special.** A feed is a space, a list is a group, a DM is a thread, the admin's front page is a layout. New behaviour comes from permissions and defaults, not from new kinds of object.

## 2. Scope

### v1

- **Spaces** (§3.1): flat, no hierarchy, created by any member. Each space says who can read it, reply in it, start threads in it and administer it, choosing for each from everyone, all members, or named groups, lists and people.
- **Feeds** (§3.2): every member has a space of their own from the moment their account exists, in which by default only they start threads and any member replies. The owner holds all of its permissions.
- **Lists** (§3.3): member-owned groups of other members, usable anywhere a group is, and invisible to anyone not on them. Every member starts with an empty "Friends" list. Site-wide groups, managed by admins and assignable through invites, remain.
- **Per-thread permissions** (§3.4): a thread may narrow who can read or reply to it, never widen.
- **Front page** (§3.5): each member arranges spaces into named sections of their own, starting from an admin-set default; `/latest` has a tab for everything, one for the member's front page and one per section; spaces can be muted; a full index of spaces exists separately.
- Flat, chronological, paginated threads, with a stable permalink for every post.
- Markdown posts with a preview; editing and deleting your own posts.
- Replying with a quote, which records an "in reply to" link to the quoted post.
- **Quote-forks:** starting a new thread that embeds an existing post, like a quote tweet, with a backlink on the original post.
- Reactions from a small set of emoji chosen by the admin.
- A per-thread read marker, "jump to first unread", and unread indicators in thread lists.
- **Few notifications** (§9): an @-mention, a reply in a thread you are subscribed to, a DM, and notices about your own account or content. You are subscribed to threads you start. Nothing subscribes you to a space.
- DMs between users, implemented as private threads with a list of participants.
- Invites as a first-class concept, a public tree of who invited whom, and invite-only registration by default.
- Accounts: email with a password or a magic link, email verification, password reset.
- Moderation (§8):
  - a report button and one review queue;
  - hide, delete, lock, pin and move threads, and move posts between threads (which covers split and merge);
  - timed silence and suspend;
  - a moderation log (the entries about public content are visible to everyone) and staff-only notes on users;
  - **break glass**: staff reading content their own permissions don't cover is a deliberate, time-limited, immutably logged action, announced at once to everyone affected.
- Permission-aware full-text search using Postgres.
- Image uploads in posts and avatars, with metadata removed and size limits.
- User profiles showing avatar, title, who invited them and a link to their feed.
- **No signatures.** Instead, each user can customise the post-info column shown to the left of their posts: a title; pronouns; a short status line of up to about 60 characters, plain text with no links; an optional location. The column always shows the avatar, display name, username and join date.
- **Edit history.** Editing has no time limit. An edited post shows "edited", which links to its revisions, and anyone who can see the post can see its revisions.
- Data export and account deletion; a simple contact and report form that works without logging in.
- Live "N new posts" indicator on threads using server-sent events.
- Deployment: docker compose, a first-run `/setup` wizard, automatic migrations, nightly backups.

### Soon after

- Open registration modes (email-verified, admin approval), with the anti-bot measures they need: a hidden honeypot field, a minimum form-fill time and a proof-of-work check.
- Passkeys and generic single sign-on (OIDC); Discord as an optional single sign-on provider.
- Rule-based sections ("new and active spaces", "feeds I'm on the Friends list of"); a "newest from each space first" sort for the front-page tabs if busy spaces drown quiet ones; nested groups; a gate on space creation (account age or posts) for sites with open registration; "similar spaces already exist" hints when creating one.
- Digest emails, Atom feeds, a webhook to post new threads elsewhere (for example into Discord), bookmarks, web push notifications, templates for terms of service, privacy and Online Safety Act paperwork, importers, spoiler markup.

### Out of scope

- ActivityPub, end-to-end encrypted DMs, real-time chat.
- Gamification and trust-level ladders. The only exception is a simple "new user" limit.
- Native apps, reply-by-email, a plugin API.
- Multiple communities in one install. Run a separate instance per community instead.
- An unread-driven "inbox" home view. Forum readers don't clear to zero.

## 3. Domain model

### 3.1 Spaces and grants

A **space** is where threads live. There is one flat set of spaces; they have no parents. Every space has a slug, a name, an optional description and a **kind**: `topic` or `feed`.

**Who can create a space:** any member, by default. A site setting (`space_creation`, on the admin settings page) restricts it to staff. There is no gate on creation while registration is invite-only; a gate for open registration is listed under "soon after". A member who makes a space lands on its settings page, where who may read, reply, start threads and administer is set; staff additionally place it in a section there.

**Four verbs.** Each space holds a **grant** for each of four verbs:

| Verb | Allows |
|---|---|
| `read` | seeing the space and its threads |
| `reply` | posting in existing threads |
| `start` | starting threads |
| `administer` | renaming, describing, changing grants, archiving; and moderating within the space: pin, lock, rename, delete threads, hide posts, move threads out |

A grant is a **level** plus optional named members:

- `public`: anyone, including anonymous visitors. Only valid for `read`.
- `members`: any logged-in user who is not suspended.
- `staff`: moderators and admins. Roles aren't groups, so this is how a grant names them (Announcements: `start` staff).
- `listed`: only the groups, lists and individual people named in the grant. A `listed` grant naming nobody lets nobody through.

Each verb's grant is independent, except that `reply` and `start` imply nothing about `read`: a viewer needs both `read` and the verb. The grant for `administer` of a new topic space names its creator; administrators can name others, and a space whose `administer` grant is empty (its only administrator left) is administrable by site staff until they name someone. Staff hold `reply` and `start` only where a grant says so, like anyone else.

**Defaults for a new topic space** **[proposal]**: `read` members, `reply` members, `start` members, `administer` the creator. A public space is an explicit choice.

**Staff and spaces.** Admins and moderators hold `administer` on every space, so they can pin, lock, move, archive and otherwise clean up anywhere, and every such action in a space they don't administer in their own right is recorded in `mod_actions`. They do **not** hold `read` anywhere by role. Any staff action that would let them read something they can't (changing a `read` grant, adding themselves to a list, viewing a reported post) is a break-glass access (§8).

**Essential spaces.** An admin can mark any number of spaces as essential. Essential spaces can't be muted, and appear at the top of every front page regardless of layout **[proposal: the "at the top" part]**. The setup wizard creates two: "Announcements" (`start`: staff) and "General".

**Lifecycle.** Administrators can **archive** a space: it becomes read-only, keeps its URL, stays searchable, and drops out of the default view of the space index. Threads can be moved out before archiving, which is how a space that didn't take off is folded back into another. A space with no administrators whose creator has been inactive on the site for 60 days can be claimed by any member through staff **[proposal]**.

**Slugs.** Space slugs and usernames share one namespace: a topic space can't take a slug equal to a current or retired username, and a new username can't equal an existing space slug. A feed's slug is its owner's username (in lower case, so it may hold underscores, which a topic space's can't) and follows it if the username changes **[proposal]**. The reason is that mentions, search and links use bare names, and Lemmy's `u/linux` versus `c/linux` shows the collision bites even when the URLs differ.

**Visibility of a thread.** Every thread stores `threads.visibility`, its **effective** visibility: `public`, `members`, `restricted` (v0.1 called this `groups`) or `private` (DMs). It is derived from the space's `read` grant and the thread's own restriction (§3.4), and updated whenever either changes or the thread moves. The anonymous path filters with a single `WHERE visibility = 'public'`.

**The "at most as visible" rule.** Destination D is at most as visible as source S if everyone who can read D can read S. For quote-forks and moves it's computed conservatively on effective audiences:

| S \ D | public | members | restricted(Rd) | private |
|---|---|---|---|---|
| public | ✓ | ✓ | ✓ | ✗ |
| members | ✗ | ✓ | ✓ | ✗ |
| restricted(Rs) | ✗ | ✗ | ✓ if Rd ⊆ Rs | ✗ |
| private | ✗ | ✗ | ✗ | ✗ |

Here R is the set of groups, lists and people named in the effective `read` grant; Rd ⊆ Rs is checked on those names, not on the people they currently contain, so it stays stable as memberships change. Content in DMs can never be quoted out, and DMs never contain quote-forks.

### 3.2 Feeds

A feed is a space of kind `feed` with an **owner**. Everything about it follows from the ordinary rules; the differences are only in defaults and lifecycle.

- **Created with the account.** Every member has a feed from the moment their account exists. Its slug is the owner's username, its name is the owner's display name.
- **Defaults**: `read` members, `reply` members, `start` the owner, `administer` the owner. The owner can change any of these through the same settings page as any space, for example `read` to their Friends list, or `start` to members if they want a guestbook.
- **Invisible until used.** A feed with no threads doesn't appear in the space index, in anyone else's sections, or in the "All" tab. It is in its owner's own "Feeds" section from the start, so the owner always sees where to post.
- **Owner-moderated.** The owner holds `administer`, so they can lock, pin and delete threads in their feed and hide replies, as LiveJournal owners could in their journals. These actions are logged like any other administration of a space.
- **Not transferable.** `administer` on a feed always includes its owner and can't be given up. When the owner's account is deleted the feed is archived: readable by whoever could read it, closed to new posts.
- **Nothing else is special.** Reactions, read markers, search, quote-forks, moves and reports work exactly as in a topic space. Thread lists, `/latest` tabs and the space index show feeds alongside topic spaces, with a kind filter on the space index.

### 3.3 Groups and lists

A **group** is a named set of members. There are two sorts, in one table:

- **Site groups** have no owner. Admins create them and manage membership, and invites can add people to them. They are visible to all members.
- **Lists** have an owner, who alone adds and removes members. A list is **hidden**: only its owner and the people on it know it exists. Every member gets an empty list called "Friends" when their account is created, and can make more from their settings page.

Both can be named in any grant: a list by its owner and by the people on it, the only ones who know it (`docs/v02-decisions.md` #66). Both are flat: a group can't contain another group **[nested groups: soon after]**.

**What people see.**
- Someone on a list sees its name and who else is on it, on a "Lists" page in their settings, and on any space or thread whose grant names it: "Visible to: alice's Friends (12)". There is no notification for being added **[proposal]**.
- Someone who can read a space or thread but isn't on a list that one of its grants names sees "people alice has chosen" rather than the list's name.
- A space's audience page, `/c/{slug}/audience`, says for each of its four grants who it lets through, to the members who can read the space: each group and list named, with who is on it for a list the viewer owns or is on (and a site group they are in), lists they aren't on as "people alice has chosen", and the people named one by one (`docs/v02-decisions.md` #64).
- The creation form says: "People on this list can see its name and who else is on it."

**Lifecycle.** A group named in any grant can't be deleted, only renamed or emptied. When a list's owner's account is deleted, the list is frozen: its membership can't change, and grants naming it keep working. Removing someone from a list removes their future access; their own posts stay where they are, attributed to them, and they keep the ability to see and edit those posts' revisions **[proposal: or not; simplest is that they lose access to the whole thread]**. Built in milestone 12 as the main text says: they lose the thread, but each of their own posts' history (`/p/{id}/revisions`, to which `/p/{id}` leads them) and edit form stay open to them, showing the post and nothing else of the thread; this holds whatever took the thread away from them, in a space's thread, while the post is live and they aren't suspended (`docs/v02-decisions.md` #58).

**Memberships and the viewer.** `UserCtx.group_ids` holds every group and list the user is in, and the lists they own: a list lets its owner in wherever it lets in the people on it, so the owner is never one of its members (`docs/v02-decisions.md` #49). Because groups are flat, a grant check is `level` logic plus one array intersection; no recursion.

### 3.4 Per-thread permissions

A thread may carry its own `read` and `reply` restriction: a set of groups, lists and people. The thread's **effective** audience is the intersection of the space's grant and the thread's restriction, so a thread can only ever be narrower than its space, and nothing has to enforce that invariant: narrowing the space narrows every thread in it, and moving a thread into a smaller space narrows it too.

- Only `read` and `reply` exist at thread level. Starting threads is a space verb; `administer` stays with the space; "nobody may reply" is the existing lock.
- The thread's author and the space's administrators can set or change the restriction, including widening it back to the space's audience.
- Repliers must be readers.
- The thread page shows a lock and the named audience next to the title and next to the reply box only when the thread is narrower than its space. A thread that simply inherits shows nothing extra.
- A DM is the limiting case: a thread with no space whose readers are its participants.

### 3.5 Front page, sections and the space index

**Layouts.** A layout is an ordered list of named **sections**, each an ordered list of spaces. There is one admin-set **default layout**, and each member has their own, copied from the default when their account is created and then theirs to edit. A member's own feed is added to a section called "Feeds" at creation. Changing the default later affects members who haven't edited theirs **[proposal, as Discourse does]**.

"Following" a space means putting it in one of your sections, and nothing more. Permissions, not following, decide what you can read and post; you never need to follow a space to take part in it.

**Default layout** created by the setup wizard: a section "Forum" with Announcements and General, and an empty section "Feeds".

**Pages.**
- `/`, for a member: essential spaces, then their sections, each listing its spaces with an unread count and the last post. For an anonymous visitor: the default layout. Each section heads to its own tab of `/latest`.
- `/latest`: threads by last activity, with tabs **All**, **Front page** (every space on your front page: the essential spaces and those in your sections) and one per section. Anonymous visitors get All and the default layout's sections.
- `/spaces`: the full index of spaces the viewer can read, sorted by creation date (newest first) by default, with sort by recent activity or name, a kind filter (topic, feed, both) and an "include archived" toggle. Feeds without threads are left out.

**Mute.** A member can mute a space. A muted space leaves their sections and is left out of All, the default view of the space index and search results (a search within that one space still finds its posts). Its URL still works and they can still post there. Essential spaces can't be muted.

**Unread.** Thread read markers are unchanged (§3.7). A space's unread count on the front page is its number of threads with unread posts. "Mark all read" is offered per space and per section.

### 3.6 Quote-forks

- A thread can have `forked_from_post_id`. Its page shows the source post embedded above the first post, the way a quote tweet shows the original.
- **Creating a fork is refused** unless the destination space is at most as visible as the source post's thread (§3.1), using the destination's `read` grant and the source's effective audience.
- **Embeds are references, not copies.** The embedded post is checked against the viewer's permissions every time the page is rendered. If the source is later moved somewhere more private, restricted, or deleted, the embed shows "this post is unavailable".
- The source post shows "↪ continued in: <title>" backlinks. Each one is shown only to viewers who can see the fork.
- Editing the source post updates every embed. For the cache, see §5.
- An embed always shows the current version of the source post, marked "edited" if it has been.

### 3.7 Threads, posts, reactions, read tracking

Unchanged from v0.1:

- Each post has `seq`, its number within the thread, starting at 1 and never reused. A deleted post becomes a tombstone, so page boundaries never shift. Pages hold 50 posts.
- A post can have `reply_to_post_id`, in the same thread; quoting sets it. Markdown is CommonMark plus GitHub extensions, rendered by comrak and sanitised by ammonia; HTML is generated on save and mentions resolved then. A mention of someone who can't read the thread produces a link but no notification.
- Edits have no time limit, each stores a revision, edits within 5 minutes merge, `/p/{id}/revisions` shows diffs, and moderators can redact a revision.
- Reactions: an admin-configured set of up to about 8 emoji; several per user per post, each once; counts shown, who-reacted on hover; no effect on order, bumping or notifications. The same in feeds as everywhere.
- `thread_reads(user_id, thread_id, last_read_seq)`: the marker only moves forward; "jump to first unread" goes to `last_read_seq + 1`; the author of a thread has read its first post.

### 3.8 Invites

Unchanged from v0.1: `invites(id, code_hash, created_by, email?, max_uses, uses, expires_at, grant_group_ids[], note, created_at, revoked_at)`; `users.invited_by_user_id` and `users.invite_id` are fixed at sign-up; who can invite is a site setting; staff can make multi-use links that add people to site groups; the tree is public at `/users/tree` and on profiles; staff see it in the user admin view and can suspend a subtree.

### 3.9 DMs

- A DM is a thread with `visibility = 'private'`, no space, and a participant list in `private_thread_participants(thread_id, user_id, joined_at, left_at)`. DMs get pages, unread markers, reactions and notifications for free.
- They never enter the anonymous cache, search for other users, Atom feeds, digests or the public moderation log.
- Safety controls: a site-wide on/off switch; a minimum account age or post count before a user can start DMs; a cap on recipients and a rate limit; a "who can DM me" setting (everyone, people I've interacted with, nobody); blocking users; and reporting a DM post.
- **Reporting a DM** works like reporting anything else (§8): the report carries no content, and a staff member who opens it is breaking glass on that conversation, which every participant is told about at once.
- Messages from staff can't be blocked.

## 4. Data model (Postgres)

IDs are `bigint` identity columns. Times are `timestamptz`. Grants hold ids in arrays; at this scale that is simpler than join tables and the access layer already works with `group_ids` arrays.

```
users(id, username UNIQUE citext, display_name, email UNIQUE citext, email_verified_at,
      password_hash?, role {user, moderator, admin}, title?, pronouns?, status_line?, location?, avatar_upload_id?,
      invited_by_user_id?, invite_id?, silenced_until?, suspended_until?,
      dm_policy, email_prefs jsonb, created_at, deleted_at?)
retired_usernames(username citext, retired_at)

groups(id, slug?, name, description, owner_user_id?, frozen_at?, created_at)   -- owner NULL: site group (with a slug); else a list
group_members(group_id, user_id, added_by, added_at)

spaces(id, slug UNIQUE citext, name, description, kind {topic, feed}, owner_user_id?,
       created_by, essential, archived_at?, thread_count, last_post_at?, version, created_at)
space_grants(space_id, verb {read, reply, start, administer}, level {public, members, listed},
             group_ids bigint[], user_ids bigint[], PK(space_id, verb))

threads(id, space_id?, visibility {public, members, restricted, private}, title, slug, author_id,
        forked_from_post_id?, read_group_ids?, read_user_ids?, reply_group_ids?, reply_user_ids?,
        last_seq, last_post_at, pinned, locked, deleted_at?, version, created_at)
private_thread_participants(thread_id, user_id, joined_at, left_at?)
posts(id, thread_id, seq, author_id, reply_to_post_id?, body_md, body_html,
      search tsvector GENERATED, created_at, edited_at?, deleted_at?, hidden_at?,
      UNIQUE(thread_id, seq))
post_revisions(id, post_id, rev, body_md, body_html, editor_id, created_at, redacted_at?, redacted_by?)
reaction_types(emoji, position, enabled)
reactions(post_id, user_id, emoji, created_at, PK(post_id, user_id, emoji))
thread_reads(user_id, thread_id, last_read_seq)

layouts(id, user_id? UNIQUE, edited_at?)          -- the row with user_id NULL is the default
layout_sections(id, layout_id, name, position)
layout_section_spaces(section_id, space_id, position)
space_mutes(user_id, space_id)
thread_subscriptions(user_id, thread_id)

notifications(id, user_id, kind, actor_id?, thread_id?, post_id?, created_at, read_at?, emailed_at?)
invites(...)                                      -- §3.8
sessions(token_hash, user_id, created_at, last_seen_at, ip, user_agent)
login_tokens(token_hash, user_id, purpose {magic_link, verify_email, reset}, expires_at, used_at?)
blocks(user_id, blocked_user_id)
reports(id, reporter_id?, contact_email?, target_kind, target_id, reason, status,
        resolved_by?, resolution?, created_at)
mod_actions(id, actor_id, action, target_kind, target_id, reason, public, details jsonb, created_at)
staff_access(id, actor_id, scope {thread, space}, thread_id?, space_id?, report_id?, reason,
             created_at, expires_at)             -- append-only; see §8
staff_access_views(access_id, path, viewed_at)   -- append-only
user_notes(id, user_id, author_id, body, created_at)
uploads(id, user_id, sha256, mime, bytes, width, height, created_at)
jobs(id, kind, payload jsonb, run_at, attempts, locked_at?, last_error?)
settings(key, value jsonb)
```

Notes:

- `threads.read_group_ids` and friends are `NULL` when the thread inherits from its space. The effective audience is computed in the access layer's SQL fragment, and `visibility` is recomputed on every change to the thread, its space's grants, or a move.
- `staff_access` and `staff_access_views` are append-only: the application's database role has no `UPDATE` or `DELETE` on them, and a trigger refuses both, so the log can't be edited from inside the application **[proposal: the trigger; the role grant alone covers the app]**.
- A single `threads` table for spaces and DMs is kept from v0.1; the `visibility` column and the access layer enforce the separation.

## 5. Rendering and caching

- **Templates:** askama, compiled into the binary.
- **CSS:** one hand-written stylesheet using CSS variables, with light and dark themes. It adapts down to 320px wide: on narrow screens, the author column collapses into a single line above each post.
- **Visual direction: "Classic × Broadsheet"** (chosen 2026-10-01; mockup 3b on the [design canvas](https://claude.ai/artifact/PPCuLhSznBYVmHTLsyTVH3)). We expect to try variants later, so all colours, fonts and spacing are CSS variables, which keeps alternative themes cheap.
  - **Structure:** the classic phpBB layout, with an Author / Message two-column post table, header bars, shaded alternate rows, boxed "X wrote:" quotes, breadcrumbs, "Go to page" links and a quick-reply box.
  - **Type:** Newsreader (a serif with optical sizes), body text about 17px. Labels, navigation and actions are in small caps with letter-spacing.
  - **Colour:** newsprint-grey page (`#ECE9E0`), off-white panels, ink-black (`#161616`) header bars with off-white small-caps labels, and one red accent (`#A1171E`) for the "new since your last visit" double rule, unread counts and the current page.
  - **Details:** posts are numbered "№ 104"; author details sit in a ruled left column; quotes are set between thin rules; post actions are text links (quote · new thread · report). A thread narrower than its space shows a small lock and its audience by name.
  - **No** rounded cards, shadows, gradients or pill buttons.
- **React "islands":**
  - Built with Vite. Each island mounts on an element with `data-island="composer"` and reads its props from a `<script type="application/json">` tag next to it.
  - v1 islands: the composer (Markdown with server-rendered preview, quote insertion, image paste), the reaction picker, the notifications menu, the live "new posts" bar, the quote-fork dialog, and the layout editor (drag spaces between sections; the plain form is add/remove/move up/down).
  - Each island replaces a working plain-HTML form.
  - Anonymous pages load no island JS.

### Anonymous cache

- **When it applies:** a `GET` request with no session cookie is served from an in-memory cache keyed by path and query. On a miss, the page is rendered with `Viewer::Anonymous` and then stored. Other spellings of a cached page's URL redirect to the canonical one, so the set of keys stays bounded.
- **Never set cookies for anonymous visitors.** The login and invite forms protect against cross-site request forgery with SameSite cookies and an `Origin` check, not CSRF tokens, so even those pages can be cached.
- **Invalidation:** each cache entry is tagged with what it depends on: `thread:N`, `space:N`, `user:N` and `site`. After each write commits, the app clears the matching tags. This happens in-process, because there is exactly one process.
  - Editing post P clears the tag of P's thread, plus every thread with `forked_from_post_id = P`.
  - Changing the default layout, an essential flag, a space's grants, or site settings clears `site`.
- **Headers:**
  - Anonymous responses: `Cache-Control: public, max-age=0, s-maxage=30, stale-while-revalidate=60`, plus an `ETag`, so a CDN in front works.
  - Everything else: `private, no-store`.
  - We never rely on `Vary: Cookie`.
  - Every response: `X-Frame-Options: DENY` and `Content-Security-Policy: frame-ancestors 'none'`, so no other site can show our pages in a frame, and `X-Content-Type-Options: nosniff`.
- **Logged-in pages are rendered fresh** on every request. They're still cheap: a few indexed queries and a compiled template. A member's front page is their layout's spaces (a few dozen at most) with unread counts, which is one query over `threads` joined to `thread_reads`.
- **Live updates:** the thread page opens a server-sent events stream for that thread, after a permission check. New posts are broadcast in-process.

## 6. Access layer

```rust
enum Viewer { Anonymous, User(UserCtx) }  // UserCtx: id, role, group_ids (site groups and lists), suspended/silenced
```

- Every content query lives in one `content` module, and every function in it takes `&Viewer`. The visibility condition is a single shared SQL fragment, which evaluates: the space's `read` grant against the viewer, then the thread's restriction if any, then (for DMs) participation.
- `role` never appears in the read condition. Staff powers are checked separately, by the handlers for staff actions, and reads under an active `staff_access` grant go through one explicit function that records the view (§8).
- Handlers can't reach the tables any other way: they only get the `content` API, not raw pool access.
- Search, notifications, email rendering, embeds and backlinks all call the same functions.

**Property-based tests (Hegel)** check the access layer. Given randomly generated spaces, groups, lists, users, threads with and without restrictions, forks, moves, grant changes and deletions:

- every item returned to a viewer is one that the reference permission function, written separately, allows;
- the anonymous-rendered HTML of every page contains no text from posts that aren't public;
- no page rendered for a staff member contains text from a thread they can't read, unless an active `staff_access` covers it and a view was recorded.

## 7. Accounts, auth, anti-abuse

- Passwords are hashed with Argon2id and are optional; users can sign in by magic link instead.
- Sessions are opaque random tokens, stored hashed in the `sessions` table, in a `HttpOnly; Secure; SameSite=Lax` cookie.
- **Registration modes in v1:** `invite_only` (the default) and `closed`.
- **Rate limits:** per IP and per account on login, magic-link requests, posting, DMs, reports, search, and creating spaces and lists. They're held in memory as token buckets.
- **New-user limits** apply until an account is 3 days old or has 5 posts, whichever comes later (both thresholds configurable). New users get lower posting and DM rates and can't start DMs. A new member's first posts can be held for approval (an admin setting, default 2).

## 8. Moderation

- **Report** is available on posts, DMs and profiles. There is also a contact and report form for people who aren't logged in. The report form tells the reporter what staff will see: the report itself, and, if staff open the content and it is private, that everyone who can read that thread will be told staff looked.
- **`/mod/queue`** lists open reports, oldest first, with one-click actions. An entry shows who reported what, where, and why; the content itself appears inline only if the staff member can already read it.
- **Actions:**
  - hide post, which keeps it for staff and shows a tombstone to everyone else;
  - delete post;
  - lock, pin, move or rename a thread;
  - move posts to another or a new thread (which covers split and merge);
  - archive a space, change its grants or its administrators;
  - silence or suspend for a duration with a reason;
  - delete as spammer, which removes the account and its content;
  - suspend an invite subtree.
- **Every action is recorded in `mod_actions`**, admins' role changes and all administration of spaces by staff included. `/moderation` shows the entries about public content to everyone, in the style of Lobsters. Entries about private content name the account and the action but carry no content.
- **Staff-only notes** on users.

### Break glass

Staff read what their own memberships allow. To read anything else, a staff member breaks glass:

1. **Trigger.** Opening reported content they can't read; opening any thread or space they can't read from a staff page; or changing a `read` grant or a list's membership in a way that would include themselves. Each of these shows one confirmation page.
2. **Confirmation.** The page names the scope (this conversation, this thread, or this whole space), asks for a reason in free text, and says who will be told. No second approver is needed: a site may have one admin. **[proposal: no re-authentication either; the session is enough]**
3. **Grant.** A `staff_access` row: who, scope, reason, the report if any, and an expiry 72 hours later **[proposal: the duration]**. Within it, the staff member reads through one access-layer function that writes a `staff_access_views` row for every page shown. After expiry the content is closed to them again.
4. **Notification, at once.** Everyone who can read the scope (participants of a DM, readers of a thread, members of a space) gets a notification and, under the normal rules, an email: "A moderator (name) viewed this conversation on 3 Oct 2026." The thread or space page shows the same line to those readers for as long as the row exists, which is forever. The notice doesn't say why; the reason is in the log. The reporter is never named.
5. **The log is immutable** (§4) and visible to all admins at `/admin/access` and to each affected person, for their own threads and spaces, at `/settings/access`. `/moderation` shows a count of break-glass accesses by month **[proposal]**.

What this doesn't cover, and the site's privacy page should say so: whoever runs the server can read the database. Parlour separates the moderator team from that person, as Zulip does; it can't remove them.

## 9. Search, notifications, email, jobs

- **Search:** a generated `tsvector` on posts and threads, queried with `websearch_to_tsquery`. The visibility filter is part of the same SQL query, and muted spaces are excluded for the muter. Search is rate-limited.
- **Notifications** are rows in the `notifications` table, of these kinds only:
  - someone @-mentioned you in a post you can read;
  - a new reply in a thread you are subscribed to. You are subscribed to threads you start; replying does not subscribe you; a toggle on the thread page does. Nothing subscribes you to a space;
  - a DM. Everyone in a conversation is subscribed to it, and may unsubscribe from it with the same toggle; a mention there still reaches them;
  - notices about you: a post of yours was hidden or approved, your role changed, a moderator viewed a private thread or space you can read (§8).
  A badge in the page header shows unread notifications; the island updates it over server-sent events.
- **Email:**
  - Sent through any SMTP relay using lettre, via a job queue in Postgres (`SKIP LOCKED`) with retries.
  - A notification is emailed only if it's still unread after N minutes and the user hasn't been seen during that time. Break-glass notices are emailed at once.
  - Emails carry one-click unsubscribe headers. The admin can send a test email.
- **Uploads:**
  - Images are stored on a volume under their content hash.
  - Each image is re-encoded to remove EXIF metadata, with limits on file size and dimensions.
  - Images are served by the app. Images attached to private content are checked against permissions.

## 10. Deployment

- **The image:** one static binary with the templates and Vite output embedded, built from a multi-stage Dockerfile into a small final image.
- **`compose.yaml`** runs `parlour`, `postgres:18` (pinned major version), `caddy` (automatic HTTPS) and `backup` (a nightly `pg_dump` plus the uploads volume, with rotation).
- **Configuration** comes from environment variables: `PARLOUR_DOMAIN`, `POSTGRES_PASSWORD`, and the SMTP settings. Everything else is set in the admin interface.
- **First run:** the app prints a one-time setup token. `/setup?token=…` creates the admin account, the site name and the SMTP settings (with a test send), creates the Announcements and General spaces and the default layout, then issues the first invites.
- **Migrations** use sqlx and run on startup, after a pre-migration dump. The app refuses to start if it detects an empty database where it expected existing data, which guards against the postgres:18 data-directory change.
- **Docs:** how to upgrade the Postgres major version, and how to restore a backup.

## 11. URLs

| Path | Page |
|---|---|
| `/` | Front page: essential spaces, then the viewer's sections (the default layout when logged out) |
| `/latest` | All threads by last activity; `/latest/front` the viewer's front page; `/latest/s/{section}` one section of the viewer's layout; `POST /latest/s/{section}/read` marks it all read |
| `/spaces` | Index of spaces, newest first; `?sort=activity|name`, `?kind=topic|feed` (both by default), `?archived=1`, `?muted=1` (a member's muted spaces too) |
| `/spaces/new` | Create a space |
| `/c/{slug}?page=n` | Space: pinned threads first, then by last activity. A feed's slug is its owner's username |
| `/c/{slug}/audience` | Who can read, reply, start threads and run the space, with groups and lists named as §3.3 says (members who can read it) |
| `/c/{slug}/settings` | Grants, name, description (administrators); for staff its section on the default front page; for admins whether it is essential |
| `/c/{slug}/archive`, `/c/{slug}/unarchive`, `/c/{slug}/delete` | Administrators' actions on a space (POST); deletion only of a space that never had threads |
| `/c/{slug}/follow`, `/c/{slug}/unfollow`, `/c/{slug}/mute`, `/c/{slug}/unmute` | A member's own arrangement (POST): put the space in a section of their front page, take it out, mute it, unmute it (`unmute` takes an optional `next`, a path on this site) |
| `/t/{id}/{slug}?page=n` | Thread |
| `/t/{id}/unread` | Redirects to the first unread post |
| `/t/{id}/subscribe`, `/t/{id}/unsubscribe` | The subscription toggle (POST), on a thread's page and on a conversation's for the people in it |
| `/t/{id}/audience` | Who can read and reply, and the per-thread restriction form |
| `/p/{id}` | Post permalink; redirects to `/t/…?page=k#p{id}` (for its author, if they can no longer read the thread, to `/p/{id}/revisions`, §3.3) |
| `/p/{id}/revisions` | Edit history |
| `/u/{username}` | Profile, linking to `/c/{username}` |
| `/users/tree` | Invite tree |
| `/search?q=` | Search |
| `/messages`, `/messages/{id}` | DMs |
| `/notifications`, `/settings`, `/invites` | Personal pages |
| `/settings/layout` | Sections and the spaces in them; mutes. Plain forms under it: `sections`, `sections/{id}`, `sections/{id}/move`, `sections/{id}/delete` (asks first), `sections/{id}/spaces`, `spaces/{id}/move`, `spaces/{id}/remove`; the island's endpoints are the same under `/api/layout` (with `sections/order` and `spaces/{id}/move` taking a section and index) |
| `/settings/lists`, `/settings/lists/{id}` | Lists you own and lists you are on, and one list (only for its owner and the people on it). Its owner's forms (POST): `/settings/lists` makes one, `/settings/lists/{id}` renames it, `{id}/members` adds someone by username, `{id}/members/{user_id}/remove`, `{id}/delete` (only a list no grant names) |
| `/settings/access` | Break-glass accesses affecting you |
| `/invite/{code}`, `/login`, `/login/{token}`, `/logout` | Auth |
| `/moderation` | Public moderation log and break-glass counts |
| `/mod/…`, `/admin/…`, `/admin/access`, `/setup` | Staff pages |
| `/admin/layout` | The default front page's editor: the same forms as `/settings/layout`, plus `spaces/{id}/essential` and `apply` (to members who haven't edited theirs); its island's endpoints are under `/api/admin/layout` |
| `/api/…` | JSON endpoints for islands (the same access layer) |
| `/events/t/{id}` | Server-sent events stream |

## 12. Repository layout

```
server/        Rust crate (axum, sqlx, askama); migrations in server/migrations
web/           Vite + React + TypeScript islands; built output embedded into server
deploy/        compose.yaml, Caddyfile, .env.example, backup script
docs/          admin guide (install, upgrade, backup/restore); prototype-decisions.md, v02-decisions.md
research/      background research
```

**Testing:**

- Rust integration tests run against real Postgres (`#[sqlx::test]` gives each test its own database).
- Hegel property tests cover the access layer, the cache and pagination.
- Playwright tests cover the main flows, with and without JS.
- CI runs all of them.

## 13. Milestones

v0.1 was built in milestones 1–8 (skeleton, reading, accounts, writing, privacy, DMs and notifications, moderation, finishing). Getting the prototype to v0.2:

9. **Spaces** (done, migrations 0021 and 0022). `categories` become `spaces` with grants derived from the old read and write levels (`administer`: staff); `category_groups` fold into the grant arrays; `threads.visibility` gains `restricted` in place of `groups`; cache tags renamed. Any member can create a space and its administrators run it from `/c/{slug}/settings`; archiving; the space index. Staff still read every space (milestone 15). Spaces don't yet have a `kind` (milestone 13) or the `essential` flag (milestone 10).
10. **Layouts** (done, migrations 0023 and 0024). The old `sections` become the default layout; each existing member gets a copy. `/` and `/latest` tabs render from layouts. Mutes. Essential spaces. The layout editor island and its plain form.
    `layouts`, `layout_sections`, `layout_section_spaces`, `space_mutes` and `spaces.essential` exist; the old sections became the default layout (spaces that were outside any section in a first section called "Spaces"), every member got a copy with an empty "Feeds" section at the end, and every new account gets one wherever accounts are made. A member who hasn't edited their copy keeps following the default (copies leave out what they have muted, and a first edit landing during a change to the default is kept). The setup wizard makes Announcements (`start`: staff) and General, both essential and public, in a "Forum" section, plus "Feeds". `/` renders from the viewer's layout (the default for visitors): essential spaces first, then the sections, each heading to its tab of `/latest`, with archived, unreadable and muted spaces and empty sections left out, and for members an unread count per space and "Mark all read" per section. `/latest` has the tabs All, Front page (members) and one per section with something in it to see, at `/latest/front` and `/latest/s/{section}`, cached for visitors. A muted space is left out of All, the default view of `/spaces` (`?muted=1` shows them) and search unless the search is within it; its page still works and says so, and members can still post there. The space page has Follow (into the first section, or a new "Following" section), Unfollow, Mute and Unmute as plain forms. `/settings/layout` is a member's editor: plain forms to add, rename, move up or down and delete (asking first) sections, add a space to a section, move a space up or down and take it off the front page, plus their muted spaces with Unmute; the layout editor island replaces the forms, dragging spaces within and between sections and off the front page (with "Move to" choices beside each space for keyboards and touch screens), saving each change through `/api/layout/…`. `/admin/layout` is the same editor for the default (`/api/admin/layout/…`), plus the essential flag per space and "Apply to members who haven't edited theirs". Any change to a member's layout marks it edited; changes to the default reach every follower as they are made. `/admin/spaces` now only lists spaces and leads to the editor; the old `/admin/sections…` routes are gone. Staff still place a space on the default front page from its settings and admins mark it essential there (both logged). The data export includes the member's front page and mutes. Decisions are in `docs/v02-decisions.md`.
11. **Notifications** (done, migration 0025). `watches` on threads become `thread_subscriptions`; category watches are dropped, with a one-time notice to the people who had them. The notification kinds are reduced to §9's list.
    The kinds are exactly `mention`, `subscription` (a new post in a thread you are subscribed to), `message` and `moderation` (notices about you); the old `reply`, `watched_thread` and `watched_space` rows became `subscription`, shown as "posted in", which was true of each. Starting a thread subscribes its author (wherever threads are made, approved held threads included); replying subscribes nobody; the thread page has Subscribe/Unsubscribe (`/t/{id}/subscribe`, `/t/{id}/unsubscribe`). Everyone in a conversation is subscribed to it, so its "Turn off notifications" toggle replaces the old Mute. Threads watched at "watching" became subscriptions, everyone in a conversation who hadn't muted it was subscribed to it, and every member who had watched or muted a space was sent a one-time notice. A mention reaches you whether or not you are subscribed, in a conversation as anywhere else, and muting a space no longer affects notifications. Email preferences are mentions, threads you subscribe to (keeping the old "watching" choice), messages and notes from the moderators; old unsubscribe links still work. Admins changing a member's role now sends them a notice. The data export lists thread subscriptions in place of watches. The property tests check who is notified of every new post against a model of subscriptions. Decisions are in `docs/v02-decisions.md`.
12. **Lists** (done, migration 0026). `groups.owner_user_id`; the Friends list for every member; grants can name lists and individual people; the settings pages; hidden-name rules in every audience display.
    `groups` gained `owner_user_id` and `frozen_at`; site groups keep their slug and unique name, and a member's lists have unique names among their own. Every member got an empty "Friends" list in the migration, and every new account gets one from a trigger, wherever accounts are made. A list lets in its owner wherever it lets in the people on it. `/settings/lists` shows the lists a member owns (with a form to make another, which says "People on this list can see its name and who else is on it."), and the lists they are on with whose each is and who else is on it; `/settings/lists/{id}` is the owner's page for renaming, adding people by username, taking them off and deleting, and a read-only page for the people on it. Nobody is told they were put on a list. A list is hidden from everyone else: its pages are not found for them (staff included), admins' group pages and invites know only site groups, and wherever a space's audience is shown (the front page, the space index, the space page, the new-thread page, the fork, move and report pages, the moderation queue, the admin's list of spaces, and the space's audience page `/c/{slug}/audience`, which the space page links to for members) a list is "Ruth’s Friends (12)" to its owner and the people on it and "people Ruth has chosen" to anyone else. Grant forms offer, for every verb, the site groups and the lists the editor owns or is on, so a list can be named by its owner and by the people on it, and keep any other list where a grant already names it (and only there) as one choice per owner, "People Ruth has chosen", which gives neither the lists' ids nor how many there are and keeps or takes out all of them. A list a grant names can't be deleted, only renamed or emptied (its page tells the owner how many spaces they can't read name it, and nobody else); deleting the owner's account (or deleting them as a spammer) freezes it, and a trigger then refuses any change to who is on it, while grants naming it keep working. Taking someone off a list ends their access at once; their posts stay, attributed to them, and they keep each post's history and edit form, which show the post and nothing else of the thread (§3.3's main text). Grants could already name individual people (milestone 9). Making lists is rate-limited. The audience page lists every group and list each of a space's grants names, with who is on a list for its owner and the people on it, and a site group's members for the people in it. The data export lists the member's own lists with who is on them and the lists they are on. The property tests model lists, owners included, in both machines and check that no page shows a list's name to anyone not on it, that the audience page names each group and list (and lists who is on it) exactly to those it should, that grant forms offer each member exactly the groups and lists they know plus one choice per owner for the named lists they don't (a space's settings page never giving those lists' ids), and that saving what a form offers names only those. Decisions are in `docs/v02-decisions.md`.
13. **Feeds** (done, migrations 0027 and 0028). A feed for every existing member and every new one; defaults; the Feeds section; profile links; the kind filter.
    `spaces` gained `kind` (`topic` or `feed`) and `owner_user_id`. Every member whose account existed got a feed in the migration, and every new account gets one from a trigger, wherever accounts are made: its address is the username in lower case (feeds' addresses may hold the underscores usernames can; topic spaces' can't), its name the display name, members read and reply, and the owner alone starts threads and runs it. It is in its owner's own "Feeds" section from the start (put there when their layout is made; the migration put it in each member's Feeds section, making one at the end for members who had taken theirs away), and copies of the default keep it there. The owner holds all of its permissions: every grant that names people names them too, the store refuses one that would leave them out ("The owner of a feed always runs it.", "The owner of a feed can always read it, reply in it and start threads in it."), a trigger makes sure (migration 0028 extended it from `administer` to every grant and mended grants saved before), and its settings page offers neither "Only moderators and admins" for any grant nor a new address. Its address follows its owner's username and its name their display name while it still has it. A feed with no thread that isn't deleted is left out of `/spaces` (its owner's included), other members' front pages and sections' tabs, the layout editor's spaces to add (except the owner's own), the search page's choice of spaces and the admin's list; its page still works, and a feed's first thread (or the loss of its last) clears the cached pages. `/spaces` takes `?kind=topic` or `?kind=feed` (both by default), and flags feeds. Profiles link to the member's feed for those who can read it, and a feed's page names its owner. A feed can't be deleted, even empty; its owner and staff can archive it; deleting the owner's account archives it for good, under "Deleted member" at the account's new name. Usernames can't be any space's address when someone joins, nor, in the setup wizard, `announcements` or `general`, which it is about to make (and setup never takes a feed for one of them). The migration moved topic spaces at a member's username to the first free address ending "-space", telling the admins and the people who ran each; migration 0028 kept that notice only for those who can read the space it names. Everything else (reactions, read markers, search, quote-forks, moves, reports) is the ordinary space behaviour. The property tests model every member's feed, its grants and their Feeds section, check that feeds are left out of the index, front pages, tabs and the editor's choices exactly while they have no threads (and never for their owners where they should see them), that cached pages follow a feed into and out of sight, and that every feed's owner always holds all of its permissions. Decisions are in `docs/v02-decisions.md`.
14. **Per-thread permissions.** The restriction columns, the effective-visibility recomputation, the audience page, the lock indicator, and the Hegel generators for all of the above.
15. **Break glass.** Staff lose readership by role. `staff_access`, the confirmation page, the view-recording read path, notifications and emails, `/admin/access`, `/settings/access`, the counts on `/moderation`, the append-only protections, and the staff-never-sees-unreadable-content property.

## 14. Open questions

1. The **[proposal]** items above: defaults for a new topic space; essential spaces pinned to the top of every front page; the 60-day claim rule for abandoned spaces; feed slugs following username changes; no notification on being added to a list; what someone removed from a list keeps; the default layout propagating to members who haven't edited theirs; no re-authentication and a 72-hour window for break glass; the public monthly count of break-glass accesses; the append-only trigger.

Resolved in v0.2 (3 October 2026): no "propose a thread, owner publishes" setting on feeds, the normal `start` grant is enough; the `/latest` tabs are plain last-activity order, and a "newest from each space first" sort is a possible later option, not a v1 feature; spaces replace categories and any member can create one; feeds are ordinary spaces owned by their member, created with the account, invisible until the first post, readable by all members by default, with only the owner starting threads; feeds get no special handling beyond that (reactions public as elsewhere); lists are member-owned groups, several per member with a default Friends list, hidden from anyone not on them; no gate on space creation while registration is invite-only; following a space means it is on your front page and nothing else; per-space mute; `/latest` has All, Front page and per-section tabs; a separate index of spaces sorted newest first; notifications are mentions, subscribed threads (authored threads by default, never by replying) and DMs; DMs stay threads; per-thread permissions as an intersection with the space's; staff hold no readership by role; break glass works with a single admin, with an immutable log visible to all admins and everyone affected, and immediate notification; reporting private content is the same report flow, with viewing as the break-glass trigger; admins can mark any number of essential spaces, Announcements and General by default.

Resolved earlier: the name is **Parlour**; no signatures, but a customisable post-info column; no time limit on editing; edit history in v1.
