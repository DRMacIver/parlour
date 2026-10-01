# 04 — The Unglamorous Essentials

Research into what a minimal self-hosted forum (Rust backend, React frontend, Postgres, docker compose on a single small VPS; public and private threads; small-to-medium communities) **must** have to survive in practice, and what can safely be left out.

Project context folded in:

- The first deployment is an existing ~500-person Discord server, possibly with a second, partially overlapping community later.
- It is open-source, general-purpose software that others self-deploy. Discord integration must therefore be **optional**. The core has to work with plain email/password and/or magic links, plus possibly generic OAuth/OIDC.
- User-to-user DMs are **in scope**. The question is how to make them minimal and safe, not whether to have them.
- Private areas are members-only categories, optionally gated to a subset of members (groups).
- Out of the box, a typical small admin needs anti-spam defaults that work **without third-party accounts**, and email that works with **any SMTP relay**.

---

## 1. Spam

### 1.1 How small forums get overrun

- **Automated engines that know your software.** XRumer and similar tools are built to post to phpBB, SMF, vBulletin and others. They can get past registration, client detection, many CAPTCHAs and email activation. Admins report bots adding 1–3 posts per second. ([Wikipedia: XRumer](https://en.wikipedia.org/wiki/XRumer), [Wikipedia: Forum spam](https://en.wikipedia.org/wiki/Forum_spam), [USENIX LEET'11: "The Nuts and Bolts of a Forum Spam Automator"](https://www.usenix.org/legacy/event/leet11/tech/full_papers/Shin.pdf))
  - A new engine gets some security through obscurity at first.
  - That stops once the software becomes common. Then the default form field names and URL paths become a fingerprint that spam tools can target.
- **Open registration on small or neglected instances.** In the February 2024 fediverse spam wave, attackers deliberately went after *small and abandoned* Mastodon/Misskey servers with open registration. Eugen Rochko's advice was to switch registration to approval mode and block disposable email providers. ([TechCrunch](https://techcrunch.com/2024/02/20/spam-attack-on-twitter-x-rival-mastodon-highlights-fediverse-vulnerabilities/), [mastodon discussion #29267](https://github.com/mastodon/mastodon/discussions/29267))
  - Lesson for us: **an unattended forum with open signup is a target**. Defaults have to be safe for an admin who has stopped paying attention.
- **LLM-written applications and posts.** Aphyr runs a ~600-member Mastodon instance that requires a written application to join.
  - An LLM-generated application using the right community vocabulary got approved.
  - The account posted commercial spam a few hours later. The same campaign hit many instances.
  - Aphyr's conclusion is that approval screening can't reliably tell such bots apart from real but isolated humans. ([aphyr.com](https://aphyr.com/posts/389-the-future-of-forums-is-lies-i-guess))
  - Implication: approval queues still help a lot, but you need defense in depth. The most robust layer is **limiting what a new account can do** (links, DMs, post rate) and **making cleanup cheap** (one-click "delete user and all their content").
- **AI scrapers as load, not spam.** Since 2024–25, aggressive AI crawlers that ignore robots.txt have overwhelmed small self-hosted sites. Anubis, a proof-of-work gate, was written in response. ([Help Net Security on Anubis](https://www.helpnetsecurity.com/2025/12/22/anubis-open-source-web-ai-firewall-protect-from-bots/), [Self-Hosted podcast #146](https://selfhosted.show/146), [Forgejo discussion](https://codeberg.org/forgejo/discussions/issues/319))
  - On a single small VPS this is a real availability risk.
  - Public thread pages need to be **cheap to serve**: server-rendered or cached HTML, sensible pagination, and per-IP rate limits on anonymous reads. They should not be a heavy SPA that fires many API calls per view.

### 1.2 What Discourse does (the most complete reference design)

Discourse's own guidance ([Tips for preventing spam](https://meta.discourse.org/t/tips-for-preventing-spam/264020), [trust levels overview](https://forum.storj.io/t/understanding-trust-levels-in-discourse/110)):

- **Trust levels.**
  - New accounts start at TL0 and are sandboxed. Discourse warns that raising the default trust level "can put your site at serious risk".
  - TL1 is reached through reading time, days visited and a few replies.
  - TL0 limits by default:
    - a capped number of links per post;
    - `newuser spam host threshold`, which limits repeated links to the same domain;
    - no personal messages (`min trust to send messages` defaults to TL1);
    - restrictions on images and attachments.
- **Community flags with automatic action.**
  - A post with enough flags is hidden automatically.
  - `num users to silence new user` defaults to 3 flags, after which the new user is silenced automatically.
  - Staff can review and reverse any automatic action.
- **Review queue.** `approve_post_count` puts a new user's first N posts into a moderation queue. "Must approve users" turns on registration approval.
- **Other tools:**
  - max new accounts per registration IP;
  - sockpuppet detection;
  - watched words (block, flag or require approval on regex matches);
  - screened IPs;
  - email-domain allow and block lists;
  - hCaptcha (a plugin);
  - an optional Akismet plugin, which Discourse now treats as replaced by AI spam detection on its hosted service.
- The [StopForumSpam plugin](https://meta.discourse.org/t/stop-forum-spam-plugin/121037) checks email, IP and username at signup and silences the account if there is a match.

### 1.3 The countermeasure toolbox, rated for a small self-hosted forum

| Countermeasure | Cost to build | Third-party dependency | Effectiveness for small forums | Notes |
|---|---|---|---|---|
| **Registration modes: open / email-verified / admin approval / invite-only / closed** | Low | None | **Very high** | phpBB has offered four modes for decades ([webnots](https://www.webnots.com/how-to-reduce-registration-spam-in-phpbb-forum/)). Approval or invite mode alone defeats almost all automated spam. Invite links fit a Discord-seeded community well. |
| **Email verification** | Low | SMTP (already needed) | Medium | XRumer-class tools can click activation links. It still filters many bots, and you need a verified address for resets anyway. |
| **New-user limits** (links, images, DMs, posts/hour, new topics/day) | Low–medium | None | **High** | The core of Discourse TL0. It caps the damage when approval is fooled, for example by LLM applications. |
| **First-N-posts moderation queue** (configurable N, default ~1–3 when registration is open) | Medium | None | **High** | Spammers rarely wait out the queue. It shares a UI with the report queue. |
| **Honeypot field + minimum form-fill time** | Very low | None | High against generic bots, zero against targeted ones | Hidden "website" field plus a signed timestamp ([spatie/laravel-honeypot](https://github.com/spatie/laravel-honeypot), [WorkOS](https://workos.com/blog/stop-bots-with-honeypots)). Use `autocomplete="off"`, `tabindex="-1"` and `aria-hidden` so screen readers and password managers don't fill it. |
| **Rate limits** (per IP and per account, on signups, posts, DMs, flags, password resets) | Low | None | High | Also protects SMTP reputation and the VPS. |
| **Self-hosted proof-of-work challenge** (e.g. ALTCHA-style) on signup and login | Low–medium | None | Medium | Raises the cost of bulk signup without third-party tracking ([comparison](https://prosopo.io/blog/hcaptcha-vs-cloudflare-turnstile)). A good *default* because it needs no account. |
| **Admin-defined Q&A challenge** ("what's the name of our Discord server?") | Very low | None | High for niche communities | phpBB's Q&A CAPTCHA was considered one of its most effective defenses when the questions were unique ([phpBB KB](https://www.phpbb.com/support/docs/en/3.1/kb/article/how-to-configure-q+a-captcha/)). |
| **Turnstile / hCaptcha** | Low | Yes (account, JS, privacy notice) | Medium–high | Offer as optional pluggable providers, not the default. |
| **StopForumSpam lookup** | Low | Free API (20k/day), or a downloadable list | Medium | Checks email, IP and username ([SFS](https://www.stopforumspam.com/faq)). No account needed for queries. Cheap optional add-on. |
| **Disposable-email domain blocklist** | Very low | Static list | Medium | Recommended by Rochko during the 2024 wave. |
| **Akismet / LLM classifiers** | Medium | Yes (paid or API) | Medium–high | Out of scope for the core. Possibly a later hook ("pre-publish check" webhook). |
| **Watched words / link-domain blocklist** | Low | None | Medium | Useful for repeat campaigns. |
| **"Delete user + all content + block email/IP" one-click** | Low | None | **Essential for cleanup** | Makes spam that gets through cheap to remove. |

**Recommended defaults for a fresh install.** None of these need a third-party account.

- Registration mode is **email-verified + admin approval**. The alternative is invite-only via invite links. The installer should ask which one.
- Honeypot and minimum form-fill time are always on.
- Built-in proof-of-work challenge on signup.
- Optional Q&A question.
- New-user limits:
  - no external links or images until first approval or N approved posts;
  - no DMs to non-staff for the first X days or until the first approved post;
  - low hourly post cap.
- Account rate limits per IP.
- A disposable-domain blocklist shipped with the software.
- An approval queue for new users' first post when registration is open.

Turnstile, hCaptcha and StopForumSpam can be switched on with a key or a toggle.

---

## 2. Moderation tooling

### 2.1 What moderators report actually using

- **The review queue is the centre of the work.** On Discourse Meta, moderators describe the merged queue (flags, queued posts, users needing approval) as the core of day-to-day moderation. They also value being able to override automatic actions there. ([Let's talk Moderation](https://meta.discourse.org/t/lets-talk-moderation/271182), [review-queue silence/suspend](https://meta.discourse.org/t/suspend-or-silence-users-directly-from-the-review-queue/410892))
- **Escalate gradually and talk privately first.** Jon Ericson (former Stack Exchange community manager) stresses three things ([Moderating on Discourse](https://jlericson.com/2020/09/15/cc_moderation.html)):
  - contact the user privately before taking public action;
  - use time-limited silences and suspensions that grow longer with repeat offences;
  - rely on community flags that act without a moderator.

  The Discourse moderation guide likewise says "Many times, a simple reminder is enough". ([Discourse Moderation Guide pt. 3](https://meta.discourse.org/t/discourse-moderation-guide-part-3-managing-content/406266)) Older admin advice agrees: ask the member to edit before you act ([CommunitySpark](http://www.communityspark.com/how-to-effectively-moderate-forums/)).
- **Content tools moderators use** ([Moderation Guide pt. 3](https://meta.discourse.org/t/discourse-moderation-guide-part-3-managing-content/406266)):
  - hide (automatic at a flag threshold, or manual);
  - delete;
  - edit;
  - move posts to a new or existing topic (this is how split and merge work);
  - close/lock;
  - topic timers;
  - slow mode;
  - staff notices on a post.
- **Silence vs suspend.**
  - Silenced users can read and log in but cannot post, flag or start messages.
  - Suspended users cannot log in, and the reason is shown on their profile.
  - Both come with a reason that only staff and the user can see. ([Discourse Meta](https://meta.discourse.org/t/suspend-or-silence-users-directly-from-the-review-queue/410892))
- **Context about a user.** Moderators on several platforms ask for:
  - **mod notes** on a user, visible only to staff;
  - **warning history**;
  - an **action log** to keep the team accountable.

  ([Reddit mod notes / mod log](https://support.reddithelp.com/hc/en-us/articles/15484384020756-Moderation-Tools-overview), [MyBB moderation features](https://mybb.com/features/moderation/))
- **Pain points.** Moderators ask for documented, predictable automation, such as how flag scores lead to hides, and a small set of simple rules (≤5).

### 2.2 Essential vs nice-to-have

**Essential (MVP):**

1. **Report/flag** on any post *and any DM*. Include a reason category and a free-text field. Reporting must work for logged-out users too, or at minimum there must be a public contact or complaint route. This is also a UK OSA requirement (§3.6).
2. **Unified review queue** containing reports, posts held for approval, and users awaiting approval. Each item has actions: approve, reject, hide, delete, dismiss, plus act on the user (silence, suspend, delete-as-spammer).
3. **Hide/soft-delete posts.** Soft delete with a reason, recoverable by staff. Hard purge for illegal content and spam.
4. **Lock/close a thread. Pin a thread. Move a thread to another category.**
5. **Move selected posts to a new or existing thread.** One primitive that covers split and merge. Medium cost, but heavily used.
6. **Silence (read-only) and suspend (no login)**, each with a duration and a reason. Plus **delete user**, with options to keep their content anonymised or purge it as spam.
7. **Moderation log (audit trail).** Who did what, when, to which object, and why. Immutable and visible to all staff. It also provides the OSA "record of decisions".
8. **Staff-only mod notes on users.**
9. **User admin view:**
   - registration IP and recent IPs;
   - email;
   - other accounts sharing the same IP;
   - post history including deleted posts;
   - reports filed by and against the user.
10. **Moderator ↔ user private channel.** A DM from staff that the user cannot block.
11. **Roles:** admin, global moderator, and preferably per-category moderators (group-based).

**Nice-to-have (soon after):**

- Formal warnings with a history.
- Slow mode and topic timers.
- Edit history visible to staff.
- Watched words.
- Bulk actions.
- Auto-hide after N flags from trusted users.
- Appeal flow. Basic appeals are *expected* by the OSA, but at MVP they can be handled by replying via the staff channel and logging the result.
- Canned removal reasons.
- Shadow features (e.g. shadowbans) are controversial and best avoided.

---

## 3. Accounts and identity

### 3.1 Registration and login flows (core, no third parties)

- **Email + password** must be supported.
  - Argon2id hashing.
  - Breached-password check is optional, because it needs an external call.
  - Rate-limited login.
- **Magic links / email OTP.**
  - Simple, and they remove password-reset handling for many users.
  - Rough edges ([Ricky Mondello](https://rmondello.com/2025/01/02/magic-links-and-passkeys/), [SuperTokens](https://supertokens.com/blog/passkeys-vs-passwords-vs-magic-links)):
    - they depend on email deliverability;
    - they open in in-app browsers;
    - they force an app switch.
  - **Recommendation:** send a magic link *plus a short one-time code* in the same email. The code avoids the in-app-browser problem. Offer it alongside passwords.
- **Passkeys (WebAuthn).**
  - Best phishing resistance.
  - Can be added as progressive enhancement through conditional-UI autofill.
  - The Rust `webauthn-rs` crate is mature.
  - Good "soon after" item. Not MVP, because recovery still falls back to email.
- **Password reset** via a single-use, time-limited link. Respond the same way whether or not the account exists, to avoid enumeration. Rate-limit it.
- **Invites.** Admin- or member-generated invite links with optional expiry, use count, and auto-join to groups. Very cheap. Ideal for moving a Discord community over and for gating private areas.

### 3.2 OAuth / OIDC and the Discord integration

- **Generic OIDC** in the core: "Log in with ___" configured from issuer, client ID and secret.
  - Covers Google, GitHub, GitLab and Microsoft.
  - Covers self-hosted IdPs (authentik, Keycloak).
  - Covers SSO for organisations.
- **Discord is not an OIDC provider.** It is plain OAuth2, so it needs a small provider-specific adapter. Make that a built-in but optional "Discord" provider.
  - Scopes ([Discord OAuth2 docs](https://docs.discord.com/developers/topics/oauth2), [User resource](https://docs.discord.com/developers/resources/user)):
    - `identify`;
    - `email`, which returns `email` and a `verified` boolean. Only trust email for account linking when `verified` is true;
    - `guilds.members.read`, which allows `GET /users/@me/guilds/{guild.id}/member` and returns the member object, including `roles` (role IDs).
  - **Guild gating:** only allow login or registration if the user is a member of configured guild IDs. Discourse core does exactly this with the `discord_trusted_guilds` setting ([Configure Discord login for Discourse](https://meta.discourse.org/t/configure-discord-login-for-discourse/127129)).
  - **Role → group mapping.** Two designs:
    1. *At login* (OAuth only, no bot). Call the member endpoint with the user's token and reconcile forum groups mapped to Discord role IDs. authentik does this, with stated caveats ([authentik Discord source](https://docs.goauthentik.io/users-sources/sources/social-logins/discord/)):
       - membership updates only on login;
       - if a later login is denied, old groups stay until removed;
       - a Discord 404 means "not a member", so clear the roles.

       To keep it fresh, refresh the token periodically, for example when a session is renewed, and re-check.
    2. *Bot-based continuous sync.* A bot token in the guild with the privileged **Server Members (GUILD_MEMBERS) intent** gets `GUILD_MEMBER_UPDATE`/`REMOVE` events.
       - Small bots can switch privileged intents on directly in the Developer Portal without verification ([intents explainer](https://gist.github.com/advaith1/e69bcc1cdd6d0087322734451f15aa2f)).
       - One 2026 report says the threshold changed from 100 servers to 10,000 users ([arkdevlabs](https://blogs.arkcore.arkdevlabs.com/discord-privileged-intents-10000-user-update)). This is unverified, but either way it is fine for a self-hosted bot in one or two guilds.
       - Without a gateway connection, a bot can also poll `GET /guilds/{guild}/members/{user}` per user from a periodic job. That is simpler for a Rust backend: no websocket, and ~500 users is trivial.
       - The Discourse ecosystem does this with plugins ([discord-rolesync](https://github.com/spirobel/discord-rolesync), [Discord Bot Construction Kit](https://meta.discourse.org/t/discord-bot-run-one-on-your-discourse-server-keep-things-in-sync/122530)), not in core.
  - **Recommendation:**
    - Core forum groups are first-class, and membership comes from one of three sources: manual, invite link, or external sync. Groups gate private categories.
    - The Discord provider records `discord_user_id` and runs a reconcile function `(user, [role_ids]) → groups`, triggered at login and by an optional periodic job with a bot token.
    - With a second, overlapping community, this extends to several configured guilds, each with its own role→group mappings. A user can belong to one guild or both.
    - Groups synced from Discord should be marked "externally managed" so moderators don't edit them by hand and then see their changes reverted.
    - **Revocation must be prompt** for private areas: leaving the guild or losing a role should remove access within the polling interval. Document this delay.
  - Account linking: a user should be able to attach Discord to an existing email account and vice versa. Avoid auto-merging by email unless both sides are verified.

### 3.3 Account deletion, GDPR and data export

- A hobby forum is **not** covered by the GDPR "household exemption". Other people are involved and the content is public, so the operator is a controller ([Irish DPC on the household exemption](https://www.dataprotection.ie/en/faqs/general/what-household-exemption)).
- UK GDPR subject access requests must be answered **within one month**, normally free of charge ([ICO](https://ico.org.uk/for-organisations/advice-for-small-organisations/subject-access-requests-sar/subject-access-request-advice/)).
- The UK ICO data protection fee (tier 1, ~£52) may apply unless an exemption covers the operator ([ICO fee guide](https://ico.org.uk/for-organisations/data-protection-fee/data-protection-fee/)). That is an operator concern, not a software one.
- **Erasure vs forum integrity.** The common approach, used by Discourse, is to **anonymise** rather than delete posts:
  - replace username and name;
  - remove email and IPs;
  - keep posts so threads stay coherent.
  - Users can delete specific posts themselves.
  - Freedom-of-expression and archiving exceptions in Art. 17(3) are generally cited to justify keeping post content.

  ([Communiteq: Discourse and GDPR](https://www.communiteq.com/discoursehosting/kb/your-discourse-forum-and-the-gdpr/), [Ethical Data Hub](https://ethicaldatahub.com/user-deletion-gdpr-forum-posts/)) This is not legal advice. Offering self-service "delete my account" with a choice of anonymise vs delete-all-my-posts is the user-friendly option.
- **Software features that make operators compliant cheaply:**
  - **Self-service data export.** A JSON/zip of profile, posts, DMs and preferences. Discourse's "Download All" is the model.
  - **Self-service account deletion/anonymisation**, which must also scrub DMs the user sent, or anonymise them in place.
  - **IP retention limit.** Configurable, e.g. 90 days, then null out IPs on old posts and sessions. Communiteq suggests "days, not months".
  - Editable privacy policy and ToS pages, shipped with templates.
  - No third-party trackers by default. If an optional CAPTCHA or OAuth provider adds one, show a notice.
  - Only strictly necessary cookies (session), so no cookie banner is needed.

### 3.4 UK Online Safety Act (OSA)

**What happened.**

- The illegal-content duties became enforceable on 17 March 2025.
- LFGSS and its host platform Microcosm (~300 small communities run by one person, Dee Kitchen) announced they would shut down on 16 March 2025. The reasons given ([LFGSS announcement](https://www.lfgss.com/conversations/401475/), [Techdirt](https://www.techdirt.com/2024/12/20/death-of-a-forum-how-the-uks-online-safety-act-is-killing-communities/)):
  - personal liability "easily weaponised by disgruntled people who are banned";
  - the need for a named senior accountable person;
  - expectations of fast moderation responses;
  - legal costs.

  I could not confirm the long-term outcome. lfgss.net now appears to host something different.
- Other forums restricted features instead. What Hi-Fi? **disabled DMs**, saying it could not moderate them well enough to be safe under the OSA ([What Hi-Fi forum](https://forums.whathifi.com/threads/disabling-direct-messages-in-the-forums.138535/post-1364950)). A small-forum owner also switched off DMs and wrote an AI-assisted risk assessment ([successfulsoftware.net](https://successfulsoftware.net/2025/07/29/the-online-safety-act-for-forum-owners/)).
- The Act remains in force. A petition to repeal it was debated in Parliament in December 2025 ([Hansard](https://hansard.parliament.uk/commons/2025-12-15/debates/DA0F7CFE-CCED-4864-BCCF-160E0AF56F92/OnlineSafetyAct2023Repeal)).

**What small, low-risk services must do.** From Russ Garrett's summary of Ofcom's codes ([Online Safety Act Notes for Small Sites](https://russ.garrett.co.uk/2024/12/17/online-safety-act-guide/)) and Ofcom/Bristows summaries ([Bristows](https://inquisitiveminds.bristows.com/post/102k11s/my-organisation-runs-a-small-online-forum-is-it-in-scope-of-the-osa)):

- An **illegal content risk assessment** across 17 kinds of priority illegal harm, written down and kept up to date.
- A **children's access assessment**. Ofcom says most user-to-user services should assume they are "likely to be accessed by children" unless they use highly effective age assurance. If children are likely to access it, a children's risk assessment is also needed (due July 2025).
- A **named accountable individual**.
- A **content moderation function** that reviews suspected illegal content and takes it down quickly.
- An **easy-to-find, easy-to-use reporting/complaints mechanism**:
  - covers specific content and general complaints;
  - lets the complainant add supporting information;
  - complaints are handled, including appeals against takedowns;
  - content is restored if an appeal succeeds.
- **Terms of service** that clearly cover illegal content and the complaints procedure.
- Removal of content and accounts linked to proscribed terrorist organisations.
- **No proactive scanning is required** of small sites.
  - CSAM hash/URL matching is recommended only for services with >700k UK users, or file-sharing services ([Russ Garrett](https://russ.garrett.co.uk/2024/12/17/online-safety-act-guide/)).
  - Note: the IWF suggests the scope of hash matching is widening for *file-sharing* services ([IWF](https://www.iwf.org.uk/policy-work/the-online-safety-act-osa-explained)). A forum with image uploads is not primarily a file-sharing service, but keep attachments modest.
- **Risk factors that raise your risk level** in Ofcom's Risk Profiles ([Ofcom risk assessment guidance](https://www.ofcom.org.uk/siteassets/resources/documents/online-safety/information-for-industry/illegal-harms/risk-assessment-guidance-and-risk-profiles.pdf?v=390984)):
  - **direct messaging**, especially to strangers, which is strongly linked to grooming;
  - image/file sharing;
  - anonymous accounts;
  - child users.
- Services at high risk of grooming are expected to stop unknown accounts from DMing child users by default.
- The 700k threshold and "low risk across all 17 harms" are what put a site in the lightest tier.

**Features that make OSA compliance cheap for operators.** This is a selling point for small UK communities, which are exactly the people who shut down.

1. A report button on every post, DM and profile, plus a **public complaint form that works without logging in**. It records category, details and timestamps.
2. Report handling that records the decision, reasoner, time and reason. This is the mod log, and it doubles as OSA record-keeping.
3. An **appeal path**: the affected user can contest a removal or suspension, it goes into the queue, and a reversal restores the content.
4. **DM controls**, which are the main lever on risk level:
   - site-wide toggle (DMs on/off);
   - "who can message me" setting (nobody / people I follow or have interacted with / members of my groups / everyone);
   - DMs reportable;
   - staff able to view reported DMs.

   An option for "DMs only between members of a shared private group" makes a Discord-like trusted setup possible.
5. Minimum-age declaration at signup, configurable, e.g. 13+ or 18+, to support the children's access assessment. **Age verification is out of scope.** Sites that would need it (porn) are not the target audience.
6. Image upload toggle and size limits, which lower risk-assessment scores.
7. **An admin "Compliance" page with templates:** ToS, privacy policy, a risk-assessment template pre-filled with the software's features and defaults, a named contact, and complaint-handling settings. Ofcom also offers an online tool and template, and a community-built record-keeping form exists ([osa-act-form](https://osa-act-form.centminmod.com/)).
8. Moderation export: a CSV/JSON of reports and actions for any period, ready if Ofcom requests it.

---

## 4. Notifications and email

### 4.1 Minimum notification set

Discourse's defaults are the best-tested model ([default notification settings](https://meta.discourse.org/t/configuring-default-notification-settings-for-users/285619), [TidBITS on Discourse notification options](https://talk.tidbits.com/t/figuring-out-discourse-notification-options/3563)):

- **In-app notifications** for:
  - replies to your posts;
  - @mentions;
  - quotes, optionally;
  - new DMs;
  - new posts in **watched threads and categories**;
  - moderation messages.
- **Thread subscription states:** watching / tracking (counts only) / normal / muted. MVP can be watching / normal / muted. Auto-watch threads you create or reply to, which is a per-user setting.
- **Email only when the user is away.** Discourse only emails if the user hasn't seen the in-app notification within a few minutes. Batch several notifications for the same thread.
- **Digest** (weekly "what you missed"). Discourse defaults to a weekly activity summary for inactive users. It is important for re-engaging an occasional forum community, which is exactly the Discord-adjacent problem: the forum's value is slower conversations people return to. Make it "soon after" if needed, but early.
- Unread tracking per thread ("new since last visit", jump to first unread). This is arguably core forum UX, not a notification.
- **Not needed for MVP:**
  - mailing-list mode;
  - reply-by-email (inbound mail is a substantial piece of infrastructure);
  - web push.

  Web push only works on iOS from a Home Screen PWA (iOS 16.4+) ([MagicBell](https://www.magicbell.com/blog/pwa-ios-limitations-safari-support-complete-guide)). Note that Apple reversed its early-2024 plan to drop EU PWA support. Web push is a reasonable "soon after" item because it needs no third party (VAPID keys are self-generated).

### 4.2 Deliverability for self-hosters

- **Do not run your own MTA on the VPS.**
  - Many VPS providers block port 25.
  - IPs on residential or cheap VPS ranges often sit on blocklists.
  - Gmail, Yahoo and Microsoft now reject mail that isn't authenticated.

  ([PowerDMARC on self-hosting email](https://powerdmarc.com/self-hosting-email/), [ari.lt](https://blog.ari.lt/b/set-up-selfhosted-email/))
- Instead, support **any SMTP relay** with host, port, STARTTLS/implicit TLS, username and password. Examples: Postmark, Amazon SES, Mailgun, Brevo, Resend, Fastmail or Proton SMTP submission, or the operator's own mail server. The Rust `lettre` crate covers this. An HTTP API is unnecessary.
- **Requirements to document and check in the admin panel:**
  - SPF, DKIM and DMARC for the sending domain;
  - Gmail and Yahoo's 2024 sender rules: SPF + DKIM + DMARC (p=none minimum) for bulk senders (≥5k/day to Gmail), spam complaint rate <0.3%;
  - **one-click unsubscribe** (`List-Unsubscribe` + `List-Unsubscribe-Post`, RFC 8058) on notification and digest mail.

  ([Resend summary](https://resend.com/blog/gmail-and-yahoo-bulk-sending-requirements-for-2024), [AWS](https://aws.amazon.com/blogs/messaging-and-targeting/an-overview-of-bulk-sender-changes-at-yahoo-gmail/))

  A small forum is under the bulk threshold. Implementing one-click unsubscribe on all non-transactional mail is still cheap and helps inbox placement.
- **Software features:**
  - a "send test email" button and a DNS/auth checker in the admin panel;
  - an outbound queue in Postgres with retry/backoff;
  - bounce handling. At minimum, mark an address as failing after hard bounces reported by SMTP. Webhook bounce processing is "soon after";
  - per-user email preferences;
  - plain text plus simple HTML;
  - never put post content from private categories in email unless the user can see it, and consider a setting to omit private content from emails entirely.
- **Email is a hard dependency at install** for verification, resets, magic links and approvals. Tolerate a "no email configured" dev mode, but warn loudly.

---

## 5. Private messaging (in scope, so design it minimal and safe)

### 5.1 Evidence

- DMs are a major vector for spam and scams. Advertisers and scammers sign up and send unsolicited messages with links or malware ([bbarchive on phpBB PMs](https://www.bbarchive.com/manage-private-messages-in-phpbb/), [TheAdminZone thread on scammers](https://www.theadminzone.com/threads/recommendations-for-stopping-scammers-on-forum.154389/)).
- Admin consensus is to **restrict new users, limit multiple recipients and attachments, and make reporting easy** rather than switch DMs off entirely.
- DMs are the main OSA risk factor for grooming (§3.4). Several UK forums switched them off rather than assess that risk.
- Moderators need to be able to message users privately, and users should not be able to block staff. This is a known Discourse bug ([users can block PMs from staff](https://meta.discourse.org/t/users-can-block-private-messages-from-forum-staff/202412)).

### 5.2 Design recommendation: "DMs are private threads"

Discourse models personal messages as **private topics** with an allow-list of users and groups, so you can add people later. Group inboxes are the same object with a group as participant. ([Discourse Meta: when to use PMs, group inboxes, secure categories](https://meta.discourse.org/t/when-to-use-personal-messages-group-inboxes-and-secure-categories/70464))

That suits this project well, because the forum already needs private threads:

- **One data model.** A thread has a visibility of public category, private category (group-gated), or **participant list**. A DM is a participant-list thread with two people. A "private thread with invited members" is the same thing with N people.
- Reuse what threads already have:
  - posting;
  - notifications;
  - unread tracking;
  - reporting;
  - moderation;
  - search, restricted to participants;
  - data export.
- **Minimal-but-safe controls (MVP):**
  1. **Admin site-wide setting:** DMs off / staff-only / members can DM.
  2. **Eligibility:** new accounts can't start DMs until they have N approved posts or X days, except to staff. Also a per-account rate limit on new conversations per hour/day, and a cap on recipients per conversation (e.g. ≤10).
  3. **Recipient control** ("who can start a DM with me"): everyone / members who share a group with me / nobody except staff. Default to "members who share a group" when groups exist, otherwise everyone (eligible users).
  4. **Block/ignore user.** The blocked user can't DM you or mention you. Their posts are collapsed for you, which is optional. Blocking staff is not allowed.
  5. **Report a DM.** Reporting gives moderators visibility of *the reported message and its conversation context*. Make this explicit in the privacy policy and UI: "Moderators can read a conversation if it is reported."
  6. **Leave conversation.** Mute is cheap too.
  7. **Links/images in DMs from new users** are subject to the same new-user limits.
  8. **Staff → user messages** cannot be blocked. Use them for warnings and appeals.
- **Don't promise end-to-end encryption.** It conflicts with report-based moderation and with OSA expectations, and it is complex. Say plainly that DMs are private from other users but not from administrators with database access. Discourse does the same.
- **Admin access to DMs.** Restrict viewing to reported conversations, plus an audited "break glass" view that is logged in the mod log. This is a good privacy and trust story.

---

## 6. Import, migration, feeds, federation

### 6.1 Import

- For **this** deployment (Discord-seeded), there is nothing to import from forum software. Importing Discord message history is not usually wanted: it is chat, not threads, and consent is unclear.
- For **general adoption**, importers matter to sites moving off phpBB/MyBB/SMF/vBulletin, which are often the ones overrun by spam or with unmaintained PHP. Discourse invested heavily here ([phpBB3 importer](https://meta.discourse.org/t/migrate-a-phpbb3-forum-to-discourse/30810), [MyBB importer](https://meta.discourse.org/t/migrate-a-mybb-forum-to-discourse/130789)). Hard parts that keep coming up in migration threads:
  - **Old URL redirects (301 permalinks).** Important for SEO and inbound links ([permalinks](https://meta.discourse.org/t/redirect-old-forum-urls-to-new-discourse-urls-using-permalinks/20930/15)).
  - **Legacy password hashes.** Store the foreign hash and upgrade it to Argon2 on first successful login ([migrated password hashes plugin](https://meta.discourse.org/t/migrated-password-hashes-support/19512)).
  - BBCode → Markdown conversion.
  - Attachments and avatars.
  - Large-dataset performance.
- **Recommendation: not MVP.** Design for it now by keeping these in the schema:
  - an `import_source`/`import_id` on users, threads and posts;
  - a `legacy_password_hash` + algorithm field;
  - a permalink/redirect table.

  Ship a **documented JSON import format** with a CLI importer first. Write phpBB→JSON as the first converter "soon after" demand appears. **Export** in the same format matters at least as much for an open-source project, both for trust and so that "can I leave?" has an answer.

### 6.2 RSS/Atom

- Cheap and well loved. Discourse exposes `.rss` on latest, category, topic, tag and user pages ([Finding Discourse RSS feeds](https://meta.discourse.org/t/finding-discourse-rss-feeds/264134)).
- **Recommendation: "soon after" (or MVP if trivial).** Provide Atom for latest threads, per category, and per thread, **public content only**. Private-category feeds would need per-user tokens, which is a later and optional addition.

### 6.3 ActivityPub federation

- NodeBB 4.0 (January 2025) federates with Lemmy, Mbin, PieFed and Discourse's ActivityPub plugin. This took substantial ongoing work to handle quirks between implementations, and a working group (ForumWG) to standardise how threaded content federates ([TechCrunch](https://techcrunch.com/2025/01/21/forum-software-nodebb-joins-the-fediverse/), [NodeBB on conversational contexts](https://community.nodebb.org/topic/18647/the-state-of-conversational-contexts-february-2025)).
- Federation brings:
  - the fediverse spam problem (§1.1);
  - remote-content moderation and defederation tooling;
  - more OSA scope;
  - background delivery queues.
- For a small community on one VPS that mostly wants private areas, it is **scope creep**.
- **Recommendation: out of scope for the foreseeable future.** Revisit only if threadiverse standards settle down. Keep stable public URLs and IDs so it stays possible.

---

## 7. Accessibility and mobile basics

- **Evidence from Discourse.** Blind users say they actively avoid recommending Discourse ([Discourse with a screen reader](https://meta.discourse.org/t/discourse-with-a-screen-reader/178105), [Screen reader accessibility issues](https://meta.discourse.org/t/screen-reader-accessibility-issues/376930)). The problems they report:
  - modals that don't take focus;
  - custom dropdowns that break;
  - no semantic structure per post (no `<article>`/`<header>`);
  - jump-to-last-read not moving focus;
  - ARIA roles added without the matching keyboard behaviour.

  Research on blind users reading discussion threads also finds high redundancy and little control over reading ([WS-DL summary](https://ws-dl.blogspot.com/2025/12/2025-12-24-beyond-alt-text-surprising.html)).
- **Infinite scroll** is a known barrier: the footer can't be reached, readers lose their position, and focus gets lost ([TheAdminBar](https://theadminbar.com/accessibility-weekly/re-think-infinite-scroll/)).
- **SPA route changes are silent** to screen readers unless focus and title are managed ([SPA accessibility](https://medium.com/design-bootcamp/accessible-by-design-part-2-your-spa-single-page-application-is-broken-for-screen-readers-739769110ce1)).

**MVP accessibility checklist (target WCAG 2.2 AA):**

- **Real URLs and pagination** for threads (e.g. `/t/123?page=4` or post-number anchors), with an optional "load more" *button*, never automatic infinite scroll. This also helps server-rendered/crawler-cheap pages (§1.1) and SEO.
- Each post is an `<article>` with a heading (author + time), so screen-reader users can skip between posts by heading or landmark. Use landmarks: `header`, `nav`, `main`, `footer`. Add a skip link.
- On client-side navigation, update `document.title` and move focus to the main heading. Announce async results (post submitted, errors) through a polite live region.
- Use native elements (`<button>`, `<select>`, `<dialog>`) instead of custom widgets, or use a well-tested headless accessible component library such as React Aria or Radix.
- Full keyboard operability with visible focus. Keyboard shortcuts are optional and must not clash with screen-reader keys.
- **Composer:** a plain `<textarea>` with Markdown and a preview is the most accessible choice. Avoid a complex rich-text editor at MVP. Show a labelled character counter and errors next to the field.
- Colour contrast, a dark mode that follows `prefers-color-scheme`, respect for `prefers-reduced-motion`, and text that scales to 200%.
- Alt-text field on image uploads.
- **Mobile:**
  - responsive layout down to 320px;
  - tap targets ≥24px (WCAG 2.2), ideally 44px;
  - the composer must not be hidden by the virtual keyboard;
  - no hover-only actions;
  - fast first paint on slow networks (server-rendered thread pages, small JS bundle);
  - a PWA manifest is cheap and enables home-screen install, and later web push on iOS.

  Native apps are out of scope.

---

## 8. Hosting and operations essentials (single small VPS, docker compose, Postgres)

These are not features, but they decide whether the software stays up:

- **Backups.** A built-in scheduled `pg_dump` plus an uploads archive, with retention and restore instructions. Optionally push to S3-compatible storage. Most small forums die from losing data, not from missing features.
- **Uploads:** local disk by default, S3-compatible optional; size and type limits; strip EXIF/GPS from images (privacy); generate thumbnails.
- **Rate limiting and caching for anonymous reads** (AI-crawler load, §1.1). Ship `robots.txt`. Optionally document how to put Anubis or a CDN in front.
- **Upgrades:** automatic DB migrations, plus a version check in the admin panel.
- **Search:** Postgres full-text search is enough for small-to-medium forums. Results must respect permissions on private categories and DMs.
- **Permission checks must be centralised.** Private categories, group gating, DMs and search all go through one "can user X see thread Y" function. That function is also needed for email, feeds, notifications and exports. Private-area leaks through side channels like these are a classic forum bug.

---

## 9. Proposed tiered feature list

### MVP must-have

| Area | Feature | Justification |
|---|---|---|
| Core | Categories (public / members-only / group-gated), threads, posts (Markdown, plain textarea + preview), edit/delete own posts | The product itself. Private areas are a stated requirement. |
| Core | Groups (manual, invite-link, or externally synced), with a single central permission function | Gating private areas; prerequisite for Discord sync; prevents leaks through search, email and feeds. |
| Core | Unread tracking, jump to first unread, paginated real URLs | Basic forum UX; accessibility; cheap to crawl. |
| Accounts | Email + password (Argon2id), email verification, password reset, magic link + one-time code | Works with no third parties; reset and verification need email anyway. |
| Accounts | Invite links (expiry, max uses, auto-join groups) | Cheapest way to move a Discord community over and to keep spam out. |
| Accounts | Generic OIDC login | One implementation covers Google/GitHub/authentik/Keycloak, which helps general adoption. |
| Accounts | **Optional** Discord provider: login, guild gating, role→group sync at login (+ periodic recheck) | Needed for the first deployment; Discourse has guild gating in core; small to build on top of groups. |
| Accounts | Self-service data export, account deletion/anonymisation, IP retention limit, editable ToS/privacy pages | GDPR baseline at low cost; avoids manual work handling requests. |
| Anti-spam | Registration modes (open / verified / approval / invite-only / closed); default = verified + approval or invite-only | The single most effective control; the 2024 fediverse wave hit open, unattended instances. |
| Anti-spam | Honeypot + minimum form-fill time, self-hosted proof-of-work challenge, optional admin Q&A, disposable-domain blocklist | Works with no third-party accounts; stops generic bots. |
| Anti-spam | New-user limits (links, images, DMs, post rate) and first-N-posts approval queue | Limits damage from targeted and LLM-written accounts that get past screening. |
| Anti-spam | Per-IP/account rate limits on signup, login, reset, posting, DMs, flags | Protects the VPS, SMTP reputation and users. |
| Moderation | Report/flag on posts, DMs and profiles; public no-login complaint form | Moderators' main input; required by the UK OSA. |
| Moderation | Unified review queue (reports, queued posts, pending users) | Where moderators actually spend their time. |
| Moderation | Hide / soft-delete / purge; lock; pin; move thread; move posts (split/merge) | Day-to-day content tools moderators actually use. |
| Moderation | Silence / suspend (timed, with reason), delete-as-spammer (content + block email/IP) | Gradual escalation; cheap spam cleanup. |
| Moderation | Immutable moderation log; staff-only user notes; user admin view with IPs and shared-IP accounts | Team accountability; context; OSA record-keeping. |
| Moderation | Roles: admin, moderator, category moderators via groups | Delegation is how small sites stay sustainable. |
| Moderation | Simple appeal path (a contest button that adds an item to the queue; reversal restores content) | OSA expectation; cheap once the queue exists. |
| DMs | DMs implemented as participant-list private threads; site toggle; new-user eligibility + rate/recipient caps; "who can DM me" setting; block/ignore; report DM; staff messages can't be blocked | User wants DMs; this is the minimal *safe* set and keeps OSA risk manageable; reuses the thread machinery. |
| Notifications | In-app notifications (reply, mention, DM, watched thread/category, moderation); watch/normal/mute per thread and category | Minimum for asynchronous conversation. |
| Email | Generic SMTP relay config, test-send button, Postgres-backed outbound queue with retry, "email only when away", one-click unsubscribe headers, per-user email preferences | Works with any relay; meets Gmail/Yahoo rules; no MTA on the VPS. |
| Compliance | Admin "Safety & compliance" page: ToS/privacy templates, named contact, minimum-age setting, OSA risk-assessment template, export of reports and actions | Directly targets the reasons UK small forums gave for shutting down; near-zero runtime cost. |
| A11y/mobile | Semantic post markup, managed focus on route change, native controls, no infinite scroll, responsive to 320px, PWA manifest, alt text | Fixing these later is expensive; Discourse's problems show what goes wrong. |
| Ops | Scheduled Postgres + upload backups, EXIF stripping, upload limits, Postgres full-text search (permission-aware), anonymous-read caching and rate limits | Small forums die from data loss and load, not missing features. |

### Soon after

| Feature | Justification |
|---|---|
| Weekly digest email | Strong re-engagement for occasional visitors. Discourse has it on by default. |
| Passkeys (WebAuthn) as progressive enhancement | Better security and UX; recovery still uses email, so not urgent. |
| Bot-token continuous Discord role sync (polling job or gateway) | Faster revocation for private areas than login-time sync. |
| Second-guild / multi-community mapping UI | Expected second community; groups model already supports it. |
| Optional Turnstile/hCaptcha providers; StopForumSpam lookup | For admins who face targeted attacks; optional third parties. |
| Watched words / domain blocklist; auto-hide after N flags; formal warnings with history; canned reasons; slow mode; topic timers | Moderator quality-of-life once the basics exist. |
| Staff-visible edit history | Disputes about "they edited it after"; moderation evidence. |
| Atom feeds (public: latest, category, thread) | Cheap; valued by power users and integrations. |
| Web push (VAPID, self-hosted) | No third party; works on iOS through a home-screen PWA. |
| Bounce processing; DNS/auth checker in admin | Deliverability hygiene. |
| JSON import/export format + CLI; phpBB/MyBB converters on demand; legacy hash upgrade; permalink redirects | Adoption by communities leaving legacy PHP forums; schema hooks planned from day one. |
| Polls, reactions/likes, bookmarks, drafts sync | Common expectations, but not needed for viability. |
| Outbound webhooks (new thread/post → Discord channel) | Strong bridge for Discord-seeded communities; simple to build. |

### Probably never / out of scope

| Feature | Why |
|---|---|
| ActivityPub federation | Large ongoing interoperability work (NodeBB needed a working group); brings federated spam and moderation load; adds OSA scope; conflicts with a private-first single-VPS design. Keep URLs stable so it stays possible. |
| End-to-end encrypted DMs | Conflicts with report-based moderation and OSA expectations; high complexity; overstated privacy guarantees. |
| Age verification / ID checks | Only needed for porn or high-risk child services; disproportionate and privacy-hostile for the target audience. |
| Proactive CSAM hash/URL scanning | Not required below 700k UK users (or for non-file-sharing services). Mitigate by limiting uploads instead. |
| Built-in Akismet/LLM spam classifiers | Third-party dependency and cost. A generic "pre-publish webhook" could allow it later without core support. |
| Running an MTA, reply-by-email, mailing-list mode | Deliverability problems; inbound mail parsing is a project of its own. |
| Native mobile apps | A responsive PWA is enough for small communities. |
| Gamification (badges, karma, levels beyond implicit new-user limits), complex trust-level ladders | Discourse's TL system is powerful but complex. A simple "new user until N approved posts / X days" rule gives most of the anti-spam benefit. |
| Real-time chat | Discord already covers this for the target community; very different scaling and moderation profile. |
| Discord message-history import | Chat isn't threads; consent and attribution problems. |
| Shadowbans | Erode trust; transparent silence/suspend is enough. |

---

## Key takeaways

1. **Registration control plus new-user limits are the anti-spam core.** Approval or invite-only registration, plus damage caps for new accounts, plus one-click spammer cleanup. All of it can work without third-party services. CAPTCHAs and StopForumSpam are optional add-ons.
2. **Moderation needs one queue and a log.** Reports, held posts and pending users go in one place. Every action is logged with a reason. Escalation is gradual, from silence to suspend.
3. **UK OSA compliance is mostly process, and software can make it cheap.** Reporting (including without login), complaint and appeal handling, records, templates, and DM controls. DMs are the biggest risk factor, so they need eligibility rules, recipient controls, blocking and reporting.
4. **DMs should be private threads.** One data model and one permission function covers DMs, invited-member private threads and group-gated categories.
5. **Discord is an optional OAuth2 adapter on top of first-class groups.** Guild gating plus role→group reconciliation at login and periodically, with revocation that propagates within a known interval.
6. **Email goes through any SMTP relay.** Never run an MTA on the VPS. Follow Gmail/Yahoo authentication and one-click-unsubscribe rules.
7. **Accessibility and crawler resilience have the same fixes.** Real paginated URLs, semantic server-rendered thread pages, and no infinite scroll.
