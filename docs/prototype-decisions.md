# Decisions made during the prototype build

The prototype was built by an automated workflow (milestones M2–M8, each one implemented, reviewed and fixed, then two final completeness passes). Wherever DESIGN.md was silent or ambiguous, the build agents chose the option closest to it and recorded the choice. This file collects those choices so they can be confirmed or changed. The full per-milestone reports are in the workflow run `wf_f82bd8fc-903`.

## Open questions

These are choices the agents made that you should confirm or change. Each one is easy to change.

1. **Holding new members' posts.** By default, a new member's first 2 posts are held for approval (`hold_posts`, set at /admin/moderation; 0 turns it off). This goes beyond DESIGN §7. Keep it, or default to 0?
2. **Hiding threads.** There is no separate "hide thread" action. Thread delete is soft, can be restored, and stays visible to staff, so it is used as hide. Is that enough?
3. **Log entries from DM reports.** A silence or suspension taken from a DM report is logged in the public log. The entry names the account and gives the reason category, but contains no DM content. Should these be staff-only by default?
4. **Image cache lifetime.** Images are served with `max-age=3600`, not the long lifetimes in DESIGN §9, so that staff removing an image takes effect within an hour. This matters for CDN and Caddy configuration.
5. **Images from other sites.** Posts can embed images from other sites, so anonymous visitors make requests to third parties. Option: allow only uploaded (`/uploads/`) images in posts.
6. **Moderators granting groups.** Moderators (not only admins) can create invites that add people to any group. Should this be admin-only?
7. **Account enumeration.** The magic-link code endpoint (`POST /login/code`) can reveal whether an email address has an account (410 vs 401 "N tries left").
8. **Leaving conversations.** Leaving a conversation is permanent: nobody, staff included, can add the person back. People can only be added to conversations that started with 3 or more people.
9. **Staff and DMs.** Staff can't read DMs they're not in unless a message is reported; reporting opens that conversation to staff permanently. This resolved the conflict between DESIGN v0.1 §3.1 ("staff see everything") and §3.7. **Superseded by DESIGN v0.2 §8** (done in milestone 15, `docs/v02-decisions.md` #108, #112): a report carries no content and opens nothing, and a staff member opening it breaks glass for 72 hours, logged and announced to the participants.
10. **Unread for new members.** Threads a member has never opened count as entirely unread, so a new member sees everything as new until they use "Mark all read".
11. **Times.** All times are shown as absolute UTC ("1 Oct 2026, 21:14"), because relative times would go stale in the anonymous cache. A site or per-user time zone setting is still open.
12. **Search language.** Search uses Postgres's `english` text-search configuration (stemming and stop words). Supporting other languages would need a migration.
13. **Public moderation log.** The log includes actions on members (silences, suspensions, spammer deletions), in the style of Lobsters.
14. **Invite links.** Invite codes are stored only as hashes, so a link is shown once when created. There is no later "Copy link" as in the mockup.

## Deviations from DESIGN.md and notable implementation choices

These are informational. Change DESIGN.md if you agree, or ask for them to be reverted.

- **ETags** are a hash of the rendered page, not built from version counters, because pages depend on more than thread and category versions.
- **Cache tags:** a fourth tag, `user:N`, was added (author post counts appear on pages). Pages are also tagged with forks or sources they don't currently show, so that visibility changes invalidate them. The cache is limited to 64 MiB, least recently used entries first.
- **Thread lists** show 30 threads a page.
- **Post counts** on the index and in thread lists include deleted posts (`sum(last_seq)`).
- **Moving posts** appends them to the END of the destination thread with new numbers. The post ids, and so `/p/{id}` links, are kept, and the old number leaves a "Moved to…" marker.
- **Magic links** show a "Log in" button rather than logging in on open, so mail scanners can't use them up. The email also contains a 6-digit code.
- **Sessions** expire after 30 days unused.
- **CSRF protection** checks Origin (or Referer) against the base URL or Host header, with no tokens.
- **Rate limits and thresholds** (posting, editing, reactions, login, reports, contact form, DMs, live streams, IP retention of 90 days) are defaults the agents chose. They're listed in the milestone reports, and most are constants or admin settings.
- **Newsreader** is self-hosted (OFL), adding about 460 KB to the binary, so visitors make no requests to Google Fonts.
- **Account deletion** keeps the row as "Deleted member", so posts stay attributed and the username can't be reused to impersonate. The email address is freed.
- **Images:** re-encoded to strip EXIF; images over 10 MB or 8000 px are refused rather than scaled; avatars are cropped to 256 px.
- **Islands build:** `web/dist` isn't committed. Cargo builds without Node, and pages simply have no islands. Release builds must run `npm ci && npm run build` in `web/` first.

## Known gaps, deliberately left out

- Deployment: production Dockerfile, `deploy/` compose and Caddyfile, backups, the pre-migration dump (DESIGN §10), admin docs, and building `DATABASE_URL` from `POSTGRES_PASSWORD`.
- There's no admin view of failed or queued jobs, and no cleanup of unused uploads or old notifications.
- The GitHub Actions CI file has never run (there's no remote).
- Tests occasionally failed on a dev-Postgres connection error (`unknown message type: '\0'`). The cause was the dev port, 54329, being an ephemeral port: now fixed by moving to 25432 (docs/v02-decisions.md #125).
- The Hegel property suite takes about 12 minutes.
