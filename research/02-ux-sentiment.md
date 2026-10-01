# What people like and dislike about forum UX

Research notes for a minimal, fast, "somewhat oldschool" self-hosted forum. The first community will be an existing Discord server of about 500 people.

**Method:** web searches plus page fetches of Hacker News, Lobsters, Tildes, LWN, Discourse Meta, Coding Horror, Slashdot/Kotaku, SpaceBattles, Quarter To Three and others (October 2026). Quotes were pulled out by a summarising fetch tool. They are attributed to the username shown on the page, but a quote may be lightly trimmed, so check the original before reusing one verbatim in public. HN comment URLs point to the comment that starts each thread; the quotes come from that thread.

---

## 1. Flat vs threaded vs hybrid

**Overall view:** this question splits people more than any other, and the split depends on what the site is for. Threading (Reddit, HN, Usenet) wins for one-off, high-volume reading where you skim and leave. Flat (phpBB, vBulletin, SA, Discourse) wins for small communities that come back every day and for long-running threads that people *follow*. Almost every community that has lasted 10+ years is flat.

### For flat
- Jeff Atwood, [*Web Discussions: Flat by Design*](https://blog.codinghorror.com/web-discussions-flat-by-design/) (2012), sets out the classic case:
  - *"Rigid hierarchy is generally not how the human mind works."*
  - New replies get lost: *"In a threaded discussion, replies can arrive any place in the tree at any time. How do you know if there are new replies?"*
  - On reading: *"As a lazy reader, I feel I've already done my part by deciding to enter the thread; after that all I should need to do is scroll or swipe down."*
  - He also notes that *"precious few threaded discussion models survive on the web"*: most forum communities older than 10 years are flat.
- HN [item 5180755](https://news.ycombinator.com/item?id=5180755):
  - polyfractal: *"I prefer a flat forum over threaded any day. Threaded forums are where discussions die."*
  - johnpowell: *"I hate nested threads. And even worse is when upvoted stuff gets pushed up to the top."*
- [Quarter To Three, "Threaded vs Flat"](https://forum.quartertothree.com/t/threaded-vs-flat-discussions/71180):
  - stusser: *"Flat works better for conversations. It constrains them, to be sure. But that's not necessarily negative."*
  - Gameoverman: *"The last thing I want to have to do is collapse or expand threaded portions."*
- Linear threads are easy to keep up with when you check in once a day, because topics are sorted by last update ([HN, "Bring back crappy forums"](https://news.ycombinator.com/item?id=48755731); [Ask HN: Are forums dead?](https://news.ycombinator.com/item?id=43259681)).

### For threaded
- HN [item 36359858](https://news.ycombinator.com/item?id=36359858), babypuncher: *"The way reddit's threads work with nested replies is also a vastly superior UX over the linear firehose approach."*
- vault_, same thread, puts a limit on that: *"Nested replies are definitely better as a way of consuming a post and its comments once and then never thinking about it again."* That is the read-once, never-return pattern, not the pattern of a community that returns daily.
- HN [5180755](https://news.ycombinator.com/item?id=5180755), jacoblyles: *"Linear forums can't achieve depth of discussion."* koralatov names flat's main problem: *"a complete mess of people quoting posts that can sometimes be several pages back."*
- From python-dev's move to Discourse ([LWN](https://lwn.net/Articles/901744/)): *"Discourse has everything linear which means that as I read it the conversation keeps jumping around, making it hard to follow."* This is the ex-mailing-list, Usenet-raised crowd.
- [Ask HN: Threaded vs Flat](https://news.ycombinator.com/item?id=43362984), Tomte: *"I was net-socialized in Usenet, strongly believe that threaded is much superior"*, though he admits he builds flat in practice.

### Hybrids
- **Quoting as soft threading.** HN [36359858](https://news.ycombinator.com/item?id=36359858), bursted: *"with quoting you're effectively having soft-nested conversations."* QT3, AustinArlitt: *"quote trees give us something like non-linear threading of posts in flatland."*
- **Discourse's reply-to.** Posts carry an "in reply to" expander, and the "N replies" count expands underneath. Atwood describes this as *"selective and judicious use of threading"* with a hard cap on depth.
- **[NoNonsense Forum](https://forum.camendesign.com/interesting_blog_post_about_threaded_vs_flat_forum_discussion_structure).** One commenter: *"threaded discussion doesn't mean you can't have a flat view on it; it's just that flat discussion means you can't have a threaded view on it."* In other words, store reply-to links even if the display is flat.
- **Slashdot.** Remembered fondly for *"collapsing low-scoring replies down to one line with a short (expandable) excerpt"* (oxygen_crisis, [HN](https://news.ycombinator.com/item?id=36359858)).

### Forking a thread from a post (the user's preferred feature)

**Prior art:**
- **Discourse "Reply as linked topic".** It starts a new topic that quotes and links the source post, and the original topic gets an automatic "continuing the discussion from…" backlink. Commenters like the idea, but almost nobody can find it ([Discourse Meta thread](https://meta.discourse.org/t/highlighting-the-reply-as-linked-topic-functionality/276569)):
  - Lhc_fl: *"I once posted a poll on my forum and concluded that about half of the people didn't know how to reply as a link topic."*
  - pfaffman (a prolific Discourse consultant): *"I can't find the reply as linked topic when I've looked… I don't think it's visible on mobile."*
  - Jonathan5: *"I think I thought it was just decoration."*
  - Wolftune: *"Important functions being effectively never discovered (even by moderators often enough!) is a really bad design."*
  - Simon makes the case for it: *"posters going off on tangents that are not directly related to a topic's OP is a good thing."*
- **Discourse "Reply as New Topic", 2013.** On launch, tomdale explained it (*"there's a 'Reply as New Topic' button to the right of every post"*). koralatov replied: *"a pretty elegant solution, but has a huge failure point: the user"* ([HN](https://news.ycombinator.com/item?id=5180755)). Atwood pointed to the moderator fallback: *"mods can click 'select posts', click click click the posts they want, then click 'move to new topic'."*
- **Moderator-only split/move.** phpBB, SMF and vBulletin have this. Admins recommend leaving a redirect or notice in the original thread so people can follow ([SMF split docs](https://www.inmotionhosting.com/support/edu/software/smf/split-topics/); [Discourse split topics](https://community.smartthings.com/t/how-to-split-topics-using-discourse-forum/10687)).
- **Admin concerns.** Some admins don't like users forking discussions into categories that have different rules ([Disable Reply as linked topic](https://meta.discourse.org/t/disable-reply-as-linked-topic-and-or-continuing-the/133412)).
- **Lessons:**
  - Put the fork action right next to Quote/Reply, label it in plain words ("Start new thread from this"), and make it work on mobile.
  - Leave a visible, in-stream backlink in the parent thread, so the fork shows up as an event in the flat timeline.
  - Let moderators do the same thing after the fact (move posts X..Y into a new thread).

---

## 2. Pagination vs infinite scroll; permalinks; jump to unread

**Overall view:** among technical and forum-loving users, Discourse-style infinite scroll is the most hated forum UX decision of the last decade. The complaints are concrete: it breaks Ctrl-F, the scrollbar, saving and printing, archiving, the back button and a sense of where you are.

- Atwood's reasoning: pagination is *"an arbitrary barrier to reading"*, so the next page should load automatically ([Coding Horror via search summary](https://blog.codinghorror.com/because-reading-is-fundamental-2/); [Discourse launch](https://www.discourse.net/2013/02/civilized-discourse-construction-kit/), which highlights replaceState URLs and jump-to-first/last).
- HN [14248405](https://news.ycombinator.com/item?id=14248405):
  - jwong_: *"Breaking ctrl+f gets my blood boiling every time I have to use a Discourse site… I need to know where the specific phrase I searched for is."*
  - gue5t: *"The scrollbar really is broken, because users expect its size to show the length of the page"*. It also becomes *"very difficult for a user to maintain an accurate mental model of the page content."*
  - Macha: *"I hate discourse boards and would much rather good old 00s vBulletin or phpBB."*
- HN [28330401](https://news.ycombinator.com/item?id=28330401) / [Nimforum thread](https://news.ycombinator.com/item?id=28329672):
  - mishac: *"The fact that discourse hijacks ctrl-f drives me insane."*
  - masklinn: *"Good forum software remains comfortable to use at million-post threads, and will render 100-post pages in a small fraction of a second."* Also: *"One decades-old forum I frequent loads its default 40-post pages in half the time it takes discourse to load a new section, and using half the data."*
  - garaetjjte, on the scrollbar warping: *"you slowly drag scrollbar… it loaded more content, and now… it again warps to the end."*
- HN [14250014](https://news.ycombinator.com/item?id=14250014): Nition found *"the Ctrl-F breaking and infinite scroll much more annoying than useful"*. Atwood's answer was *"simply press CTRL+F twice."* Users read this as dismissive.
- General [Ask HN: stop making infinite scrolling websites](https://news.ycombinator.com/item?id=9416017):
  - Infinite scroll is fine for ephemeral browsing, bad for reference. The OP: *"If I am using a website for anything different from leisure, I want to use its content as a reference."*
  - Memory crashes and unreachable footers are also cited.
- Archiving: Discourse threads don't archive cleanly in the Wayback Machine ([Discourse Meta](https://meta.discourse.org/t/make-discourse-play-nice-with-the-wayback-machine/34579?page=2)).
- **Credit where due:** Discourse's per-user read position is widely praised (see section 3). Pagination-lovers accept permalinks to posts and jump to first unread as essential. They only object to the endless scroll.

**What people want:** real pages of a fixed size (around 25–50 posts), a stable permalink per post (`/t/123#p456`, or `?page=N#p456`), the native scrollbar, and a "first unread" link from the thread list. Optionally a "view all on one page" mode for Ctrl-F and printing.

---

## 3. Unread tracking, bookmarks, subscriptions, notifications, email

**Overall view:** per-thread read tracking, plus a "jump to first unread post" link, is the one old-forum feature everyone agrees on. It is what makes "check in once a day" work, and it is what Discord and Reddit lack.

- Something Awful's "last read" bookmarks are central enough that the community built and maintains the SALR extension and the Awful iOS app around them ([SALR](https://chrome.google.com/webstore/detail/something-awful-last-read/bogegdelcjhoaakaepmoglademmhiboo); [Awful.app](https://github.com/Awful/Awful.app)).
- Usenet's `.newsrc` read tracking is still cited as the gold standard (throw0101a, [HN](https://news.ycombinator.com/item?id=48755731)).
- Discourse's tracking is popular, even with people who otherwise dislike Discourse. Lee_Ars (Ars Technica senior editor, [Discourse Meta](https://meta.discourse.org/t/new-posts-read-posts-unread-posts-i-like-this/7618)): *"I feel like I can manage where I am in all the ongoing conversations, but not have to occasionally hit the 'I GIVE UP, MARK ALL READ' button."* Atwood in the same thread: *"having a 'mark all read' button is the same as a we suck at designing software and we give up button."* (Old-forum users still use and expect mark-all-read.)
- There is a [Discourse Meta request](https://meta.discourse.org/t/remembered-topic-location-should-be-first-unread-post-instead-of-last-read-post/21440) to return to the *first unread* post, not the last read one. Small details matter here.
- **Notifications backlash:**
  - Discourse badge and trust-level notifications annoy people: *"a bulk of unproductive distracting system messages about jubilees or that I have reached a new level"* ([Discourse Meta](https://meta.discourse.org/t/deactivating-badge-and-trust-level-notifications/224507?tl=en)).
  - Watched-topic email floods are a recurring complaint ([Meta](https://meta.discourse.org/t/please-add-option-to-send-only-one-notification-email-per-watched-post-per-user-until-he-returns/22885); [Meta: excessive emails](https://meta.discourse.org/t/help-needed-discourse-sending-excessive-email-notifications/347391)).
- **Email.** A real minority, mostly open-source and mailing-list people, wants to read and reply entirely by email:
  - Discourse's "mailing list mode" exists because of them ([LWN: A farewell to email](https://lwn.net/Articles/768483/); [Icinga](https://community.icinga.com/t/discourse-notifications-digest-and-mailing-list-mode/188)).
  - Máirín Duffy: email *"comes to the reader rather than having to be explicitly looked for."*
  - One community reported about two-thirds wanting per-post email and the rest a daily digest ([search summary of Discourse Meta](https://meta.discourse.org/t/complete-daily-or-weekly-digest-for-some-users/314712)).
- **Mobile push.** dbbk ([HN](https://news.ycombinator.com/item?id=31923377)): *"forums don't come with their own app and notifications out of the box."* This is one reason people drifted to Reddit and Discord.

---

## 4. Information density, page weight, JS, mobile

**Overall view:** the technical audience loudly wants density and speed and dislikes SPAs. A minority prefers airier layouts, and mobile usability is not optional.

- old.reddit is the reference case. toyg ([HN 26100976](https://news.ycombinator.com/item?id=26100976)) lists *"Information density, load times, speed of navigation, UX simplicity, effort required client-side."* apaprocki: *"an order of magnitude faster than the main site."* Many people promise to leave when old.reddit dies ([HN 27667574](https://news.ycombinator.com/item?id=27667574)).
- Discourse density. notreallyserio ([HN 29016033](https://news.ycombinator.com/item?id=29016033)): *"Everything is twice as big as it needs to be… The excess white space and lack of differentiation between bits of content just make it harder to follow conversations."* danr4: *"95% of the times I stumble on a Discourse forum I immediately close the tab."* mostlysimilar: *"There's something about Discourse that makes the messages feel transient."*
- Page weight ([Nimforum HN](https://news.ycombinator.com/item?id=28329672)):
  - freediver: Nimforum loads *"4MB of data… to basically communicate 3KB of information"*, while FluxBB does it in *"only 100KB."*
  - zelphirkalt on Discourse: *"No JS no joy. You see a completely white page."*
  - emptysongglass: *"Can someone point me to forum/message board software that doesn't use JS at all? Like Sourcehut but for forums."*
- HalfwayToDice ([HN 14250014](https://news.ycombinator.com/item?id=14250014)): Discourse is *"an over-engineered, over-complicated, bloated, heavy mess."* Server needs of 1–2 GB RAM are also a common complaint for self-hosters.
- **Dissent:**
  - sullyj3 ([HN](https://news.ycombinator.com/item?id=28329672)): *"So many programmers have this obsession with interfaces having every available piece of information displayed all at once. I find these sorts of interfaces confusing and overwhelming."*
  - davidcollantes and weatherlight ([HN 29016033](https://news.ycombinator.com/item?id=29016033)) like Discourse and *"dislike paginated, hierarchical forums"*. They say it *"strike[s] a good balance on desktop and mobile."*
- **Mobile.** This was the old forums' real weakness:
  - jasode ([HN 39559997](https://news.ycombinator.com/item?id=39559997)): navigating via separate URLs per topic is *"very high-friction — especially on mobile."* raytopia: *"many forums can't be used very well on mobile."*
  - Tapatalk existed only because vBulletin wasn't responsive, and was disliked ([HammockForums](https://www.hammockforums.net/forum/showthread.php/167699-Request-for-Mobile-Friendly-Forum-Access-Without-Tapatalk)).
  - The rip.so signatures essay also blames signatures partly for bad mobile reading.
  - Also: *"There is no usable standard scroll bar on mobile"* (bhaak, on Discourse's custom scrubber).

---

## 5. Thread lists, sorting, stickies, categories vs tags

- **Sort by last activity** is the core of the flat-forum experience (see section 1). People like that old threads come back to life, and that 20-year-old threads stay readable in order (Findecanor, [HN](https://news.ycombinator.com/item?id=48755731)).
- **Categories.** lazypenguin ([HN](https://news.ycombinator.com/item?id=28329672)): *"I prefer the old style \*BB interface with explicit categories/sections."* foresto compares Discourse's "Latest" view to *"standing on a riverbank and watching random things float by… when I need to unearth specific information… I long for a more traditional forum."*
- **Categories vs tags.** The consensus on [Flarum](https://discuss.flarum.org/d/23608-tags-vs-categories) and [Discourse Meta](https://meta.discourse.org/t/categories-vs-tags/46935) is to use both: a small set of mandatory categories ("places") and optional tags. Admins warn against over-splitting into subforums; too many empty subforums make a site look dead ([Adjourn](https://adjourn.audent.io/organizing-your-community-topics-and-forum-structure-b49b1a312207/)).
- **Lobsters' fixed tag list plus per-user tag filtering** is liked (inactive-user: *"I filter it out in my feed"*; [Lobsters](https://lobste.rs/s/bxuqzy/why_lobste_rs_is_better_than_hacker_news)).
- **Discord forum channels** confirm the same needs from the Discord side. They exist to stop *"fast-scrolling chat walls"*, and fans like that threads *"auto-hide after inactivity"* and *"show up again to thread followers if the conversation is revived"* ([astrogation](https://astrogation.substack.com/p/why-forum-channels-are-amazing); [TechCrunch](https://techcrunch.com/2022/09/14/discord-forum-channels)).

---

## 6. Identity: avatars, signatures, titles, post counts, profiles

**Overall view:** the identity "texture" of old forums is strongly missed, specifically because it made regulars *recognisable*. Signatures in particular also caused clutter.

- al_borland ([HN](https://news.ycombinator.com/item?id=48755731)): with avatars and signatures *"it was easier to recognize people and see them as a person instead of just an opponent."* shoobiedoo liked *"avatars and custom signatures and titles to give your online persona a little bit of personality and flair."*
- al_borland ([HN 31923377](https://news.ycombinator.com/item?id=31923377)): *"I knew all the main posters… speaking via that forum nearly every day for over a decade."*
- swah, same thread, on the flat layout: *"everyone can have their 100 vertical pixels to say something… gives it a feel of 'contributing'"*. On Reddit, by contrast, *"one feels ignored most of the time."*
- [rip.so, "Forum signatures (1995–2015)"](https://rip.so/forum-signatures.html):
  - Benefits: *"you knew who someone was before you read their post"*; quote-collection sigs were *"a living document of community history."*
  - Downsides: *"signatures cluttered threads, used bandwidth, and made mobile reading harder."* Discourse disables them by default.
- **Something Awful** custom titles are a culture in themselves: paid titles, and other users can pay to change yours ([HN 33420619](https://news.ycombinator.com/item?id=33420619)).
- **Post counts and rank titles** ("Junior Member" etc.) are remembered fondly by some. Others see them as an incentive to spam (implied by the like-button and gamification threads below).
- **Lobsters invite tree** is a form of identity and accountability that users like ([Lobsters](https://lobste.rs/s/bxuqzy/why_lobste_rs_is_better_than_hacker_news)).

---

## 7. Quoting, markup, editing, edit history

- **Quoting** is the main structural tool in flat forums, and multi-quote (vBulletin) is missed (*"shame that's not obvious like the old multiquote was"* — [vBulletin forum](https://forum.vbulletin.com/forum/vbulletin-6/support-questions/4502447-multi-quote)). Overlong nested quote pyramids are the recurring complaint (koralatov, above). Quotes that link back to the source post are what make flat threads navigable.
- **BBCode vs Markdown.** Markdown has largely won with the general and Discord-trained public: *"the modern way of formatting used by Reddit, Discord"* ([Steam suggestion](https://steamcommunity.com/discussions/forum/10/135513901704863131/); [Vivaldi](https://forum.vivaldi.net/topic/13061/bbcode-compared-to-markdown)). BBCode is called *"friggin horrible to write in."* Objections to Markdown are that it lacks forum-specific constructs (quotes with attribution, spoilers) and has escaping surprises. Many suggest Markdown plus a few toolbar buttons rather than full WYSIWYG ([phpBB forum on Markdown](https://www.phpbb.com/community/viewtopic.php?t=2201646)). For a Discord-origin community, **Discord-flavoured Markdown** (`**bold**`, `||spoiler||`, `> quote`, code fences) is what people already type.
- **Editing.**
  - People accept a short "ninja edit" grace window, about 5 minutes in Discourse.
  - They want an "edited" marker after that.
  - Some want public edit history so meaning can't be changed silently ([Privacy Guides](https://discuss.privacyguides.net/t/why-isnt-post-edit-history-visible-on-this-forum/33039); [TV Tropes](https://tvtropes.org/pmwiki/posts.php?discussion=14309645440A60632400&page=1)).
  - Others want history visible only to mods, for privacy.

---

## 8. Search

- Forum-internal search has a bad reputation. phpBB users say Google site search beats the built-in one, and large boards have turned built-in search off for load reasons ([phpBB](https://www.phpbb.com/community/viewtopic.php?t=2144978)).
- Search is also the main reason people dislike moving to Discord. Luke Plunkett, [Kotaku via Slashdot](https://tech.slashdot.org/story/21/09/20/2013204/please-stop-closing-forums-and-moving-people-to-discord): *"Forums create a record, an archive we can search through."* HN: *"isn't normally publicly searchable"* ([44052237](https://news.ycombinator.com/item?id=44052237)); mooreds: *"lack of discoverability, basically forever"* ([36390647](https://news.ycombinator.com/item?id=36390647)).
- **Two separate needs:** being findable from outside (crawlable server-rendered HTML, stable URLs) and good internal full-text search that can filter by author, category and date. Ctrl-F within a thread page is a third, which infinite scroll breaks.

---

## 9. Likes, reactions, gamification

**Overall view:** this is divisive. Light reactions are tolerated or liked as a way to cut down "+1" posts. Visible scores, downvotes and badges are widely disliked by the "oldschool" audience.

- Arguments against likes/votes:
  - Austiiiiii ([HN](https://news.ycombinator.com/item?id=48755731)): downvotes create a *"chilling effect on free discussion."*
  - cucumber3732842 says the vote-tree format *"incentivizes all sorts of bad drive-by contribution behavior."*
  - Heliostorm ([SpaceBattles](https://forums.spacebattles.com/threads/why-do-you-think-forum-culture-died-how-can-we-bring-it-back.947089/)): people now want *"a select handful of posts selected via upvotes or likes"* rather than discussion.
  - ffsm8 ([HN 39559997](https://news.ycombinator.com/item?id=39559997)): platforms won by encouraging *"low effort postings"* through easy voting.
  - Forum admins report likes being used for alt-account boosting and grudge voting, and readers counting likes instead of reading. Steam deliberately left likes out ([Giant Bomb](https://www.giantbomb.com/forums/general-discussion-30/like-button-on-gb-forums-385889/); [Steam suggestion](https://steamcommunity.com/discussions/forum/10/1782766638534661909)).
- Arguments for: likes *"reduce forum clutter as instead of everyone chiming in on the same response, they can just like"* (same sources). MetaFilter "favorites" are a long-standing, mostly accepted middle ground ([MetaTalk](https://metatalk.metafilter.com/26595/Is-it-time-to-revisit-favorites)).
- Discourse badges and trust levels:
  - Badge noise is a common complaint (*"They make newcomers feel unaccomplished and rub it in everyone's face"*; [Discourse Meta](https://meta.discourse.org/t/badges-and-general-chattiness-of-discourse/187971/33?tl=en); [First Like badge](https://meta.discourse.org/t/first-like-badge-cannot-be-removed/382109)).
  - Trust levels *as spam control* (new users can't post links or images and are rate-limited) are well regarded by admins ([Understanding Trust Levels](https://blog.discourse.org/2018/06/understanding-discourse-trust-levels/)). The python-dev move also cited the *"graduated trust model"* ([LWN](https://lwn.net/Articles/768483/)).
  - Users like the quiet sandboxing and dislike the public, chatty game layer.
- Lobsters requires a reason for downvotes (classichasclass: *"this makes people more willing to engage"*).
- MetaFilter's founder credits community over karma ([Wikipedia](https://en.wikipedia.org/wiki/MetaFilter)).

---

## 10. Moderation from the user's side

- **Transparency is valued.** Lobsters' public moderation log: *"much more honest and transparent"* (pushcx). Lobsters also has no shadowbanning: banned profiles say who banned them and why ([Lobsters](https://lobste.rs/s/bxuqzy/why_lobste_rs_is_better_than_hacker_news); [about](https://lobste.rs/about)). SA's public "rap sheet" (nebqr: *"you can look at anyone's rap sheet and see exactly the post that they got banned for"*, [HN 33420619](https://news.ycombinator.com/item?id=33420619)).
- **Friction at signup works.** MetaFilter's $5 fee: *"great at keeping out low-effort drive-by spammers and trolls"* (Jordan117, [Tildes](https://tildes.net/~tech/1m2a/does_metafilters_5_entry_fee_succeed_in_enforcing_good_behaviour_also_metafilter_is_small)). SA's $10 makes permabans stick (MonkeyMalarky: *"Permabans on other platforms don't work when you can make a new email and sign up again"*). The cost: *"a weird cliquey mess… hurt their growth"* (same thread).
- **Moderation burden** (spam, running a site around the clock) is the most cited *operational* reason forums lost to Discord and Reddit (marginalia_nu, [HN 36390647](https://news.ycombinator.com/item?id=36390647); pers0n, [HN 39559997](https://news.ycombinator.com/item?id=39559997); Brett Cannon on mailing lists, [LWN](https://lwn.net/Articles/768483/)).
- **Engaged moderators.** PaulHoule: *"moderators who lean in and help new users feel included and get ahead of toxic behavior"* ([HN 31923377](https://news.ycombinator.com/item?id=31923377)).
- **User-side tools.** An ignore list or killfile is a long-standing expectation ([Kill file](https://en.wikipedia.org/wiki/Kill_file)). Flagging, thread locking, and splitting or moving posts are basic moderator needs.

---

## 11. Why forums lost, and the Discord question

### Why they lost
1. **Signup friction and fragmentation.**
   - *"Forums died because we collectively decided that signing up for accounts was too much friction"* ([Twilightsucks](https://twilightsucks.com/when-discord-killed-forums-and-nobody-noticed/)).
   - kitsunesoba: *"Signup fatigue is real"*; Discord is *"one account, infinite communities"* ([HN 36390647](https://news.ycombinator.com/item?id=36390647)).
   - Dalewyn: centralisation is wanted by people *"who have bigger fish to fry than keeping track of so many forums"* ([HN 39559997](https://news.ycombinator.com/item?id=39559997)).
2. **Aggregation.** muzani: *"Reddit is free, comes with moderation tools, and everyone has a reddit account"* ([HN 43259681](https://news.ycombinator.com/item?id=43259681)). ProgrammerPoe: *"Reddit killed the Internet forum by marketing itself a replacement to them"* ([SpaceBattles](https://forums.spacebattles.com/threads/why-do-you-think-forum-culture-died-how-can-we-bring-it-back.947089/)).
3. **Mobile and notifications** (section 4).
4. **Cost and moderation burden** (section 10).
5. **Search engines deprioritising forums** (Bender, [HN 31923377](https://news.ycombinator.com/item?id=31923377)).
6. Real numbers from one admin (dairylee, same thread): 110k posts in 2009 fell to 17k in 2021 as users moved to Steam, Reddit, Twitter and Discord.

### What people miss
- Persistence and searchability; *"Forums are more deliberate, more considered… more permanent"* (Plunkett).
- Long-form posting (*"longer content than 120 characters"*).
- Knowing the regulars, and a small scale (iillexial: *"forums used to be much smaller than subreddits. Also… forums UX was much better"*).
- Owning your community: *"they traded ownership for accessibility… When that Discord server collapsed, everything vanished"* (Twilightsucks).
- Lurkers becoming contributors. TeMPOraL: *"good contributors don't come out of nowhere — they start as lurkers"* ([HN 36390647](https://news.ycombinator.com/item?id=36390647)).

### Discord vs forum, and moving a Discord community
- Discord wins on real-time chat, voice and screen sharing, zero signup for people already on it, and outsourced spam control (SirMaster, kitsunesoba, marginalia_nu, [HN 36390647](https://news.ycombinator.com/item?id=36390647)).
- cowsup says moderators care about *"people who participate"*, not lurkers, which is why they choose Discord.
- Discord loses on async discussion. Rapidfyrez: *"In Discord it's very easy to 'talk over' someone… a very frustrating and non-constructive form of discussion"* ([SpaceBattles](https://forums.spacebattles.com/threads/why-do-you-think-forum-culture-died-how-can-we-bring-it-back.947089/)). whateveracct: *"So many discords would be better as forums"* ([HN 31923377](https://news.ycombinator.com/item?id=31923377)).
- Moving people over (Discourse Meta: [switch advice](https://meta.discourse.org/t/looking-for-advice-does-it-make-sense-to-switch-from-discord-to-discourse/374204), [small migration](https://meta.discourse.org/t/migrating-small-community-from-discord-to-discourse/351118)):
  - *"Getting early buy-in is key"* (HAWK).
  - *"Try getting a few of the most active users onto the forum and get it 'up and going'. Users may move more towards the place where there is increased activity"* (NateDhaliwal).
  - Younger audiences (teens) are hard to move. Older ones were *"likely a poor fit for Discord in the first place"* (Manuel/pfaffman).
  - Some communities tried and stayed on Discord.
  - Discord message history is hard to export, and attachment links expire (wesochuck).
- **Coexistence is the common successful pattern:** chat for chatter, a forum for durable discussion. Recommended bridges are "Login with Discord" (now in Discourse core, optionally limited to guild members — [Discourse Meta](https://meta.discourse.org/t/configure-discord-login-for-discourse/127129)) and bots that post new forum threads into a Discord channel ([discourse-discord-sync](https://github.com/barreeeiroo/discourse-discord-sync)).
- Zulip's "every message has a topic" model is often cited as a middle ground that makes chat browsable ([Zulip](https://zulip.com/why-zulip/)). It is useful evidence that people want *named, resumable threads*.

---

## Implications for a minimal forum

### Do
1. **Flat, chronological threads as the default**, sorted by last activity in the thread list. This matches the user's preference and the evidence on long-lived communities.
2. **Store reply-to and quote links even if the display is flat.** Show "in reply to @x" with a link or expand-in-place, and show "N replies" under a post. This takes most of the sting out of flat's "quote from 3 pages ago" problem without nesting.
3. **First-class "fork thread from this post".**
   - Put it next to Quote/Reply, label it in plain words, and make it work on mobile. Discourse's version failed through poor discoverability, not a bad concept.
   - The new thread opens with the quoted post and a link back.
   - The parent thread gets an inline marker ("↳ @alice started *New thread title* from this post"), so the fork is visible in the timeline.
   - Give moderators the same thing after the fact ("move posts N..M to new thread", leaving a stub).
4. **Real pagination plus permalinks.** Fixed-size pages, `#post-id` anchors that resolve to the right page, native scrollbar, Ctrl-F works. Possibly a "?all" single-page view for printing, archiving and search.
5. **Server-side read tracking per user per thread.**
   - Unread counts and bold for new posts in the thread list.
   - A "first unread" link that lands on the first *unread* post, not the last read one.
   - "Mark all read" (oldschool users expect it).
6. **Watch/subscribe and bookmarks, with restrained notifications.** On-site notifications only for direct replies, mentions, quotes and watched threads. Email that is opt-in and batched (one mail until you return, or a daily digest). Optionally reply-by-email or a mailing-list mode later.
7. **Server-rendered HTML that works with JS off.** JS only as progressive enhancement (quote selection, live preview, "new posts since you loaded" notice). Aim for tens of KB per page. Crawlable public pages (if the community wants to be public) are the core reason to have a forum rather than Discord.
8. **Dense but readable, mobile-first CSS.** old.reddit / HN / phpBB density on desktop, without the 2005 "pinch and zoom" problem on phones. No Tapatalk-style app needed.
9. **Light identity.** Avatar, display name, optional short custom title, join date, and maybe post count. A profile page listing a user's posts. Small text-only signatures are a maybe, off or collapsed by default. The goal is that regulars are recognisable.
10. **Markdown in Discord's flavour**, so it matches what the community already types. Spoilers, quote blocks with attribution and links, code. Live preview. No WYSIWYG.
11. **Edits:** grace window, then an "edited" marker, with history visible at least to moderators (decide whether public).
12. **Search:** full-text search with author, category and date filters. Postgres FTS or SQLite FTS5 is plenty at 500 users.
13. **Moderation:** lock, pin, move, split, delete with a reason, and a **public moderation log** (Lobsters). User ignore list. Report/flag button. Rate limits and a link/image restriction for brand-new accounts instead of a visible trust-level game.
14. **For the Discord transition:**
    - "Log in with Discord" (optionally restricted to guild members) to remove signup friction.
    - A webhook that posts new threads and/or digests into a Discord channel.
    - Seed the forum with the most active members first.
    - Present it as a place for durable or async topics *alongside* Discord, not a replacement.

### Avoid
- **Infinite scroll, custom scrollbars, Ctrl-F hijacking, client-side-only rendering.** These are the most-cited Discourse complaints.
- **Badges, trust-level announcements and "you've earned…" notifications.** Gamification is the clearest "hated" item among the target audience.
- **Downvotes and visible scores, and reordering posts by votes in a thread.** If you add anything, a single low-key reaction or "+1" (possibly with counts hidden or shown only on hover) to cut down "me too" posts.
- **Deep nesting or tree display.** At most one level of "reply-to" context.
- **Too many categories or subforums at launch.** A 500-person community should start with a handful.
- **Heavy server requirements.** Self-hosters resent Discourse's 1–2 GB RAM floor. A single binary with SQLite fits the "oldschool, fast" goal.
- **Chatty email** (one email per post by default).
- **Hiding key actions behind icons** (lesson from reply-as-linked-topic).
- **Shadowbans and silent moderation.**
