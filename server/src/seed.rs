//! `parlour seed`: fills an empty database with sample content for
//! development, in the style of a small hobby club ("The Tuesday Club"):
//! sections, public and private categories, long threads that run to
//! several pages, quote-replies, a quote-fork, and deleted, hidden and
//! edited posts.
//!
//! The text is generated from fixed sentence lists with a fixed random seed,
//! so every run produces the same posts. Timestamps are relative to now.
//!
//! Seeding runs in one transaction, so a failure leaves nothing behind. It
//! records the highest id in each content table in the `dev_seed` setting,
//! and `parlour seed --reset` refuses to delete anything unless that record
//! still matches: it only ever wipes a database that the seed command
//! filled and that nobody has added to since. The binary ships to
//! production, where running `--reset` out of habit must not be able to
//! destroy a real forum.

use std::fmt;

use chrono::{DateTime, Duration, DurationRound, Utc};
use sqlx::{PgConnection, PgPool};

use crate::markdown;
use crate::store::{self, CreatedThread, NewCategory, NewPost, NewThread, NewUser};

#[derive(Debug)]
pub enum SeedError {
    /// The database already has forum content.
    NotEmpty,
    /// `--reset` was asked to wipe a database that `parlour seed` didn't fill.
    NotSeeded,
    /// `--reset` was asked to wipe a seeded database that has gained users or
    /// content since.
    ChangedSinceSeed,
    Database(sqlx::Error),
}

impl fmt::Display for SeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SeedError::NotEmpty => write!(
                f,
                "the database already has categories or users; run `parlour seed --reset` to replace them"
            ),
            SeedError::NotSeeded => write!(
                f,
                "refusing to reset: this database has users or content that `parlour seed` didn't create"
            ),
            SeedError::ChangedSinceSeed => write!(
                f,
                "refusing to reset: users or content have been added since this database was seeded"
            ),
            SeedError::Database(err) => write!(f, "database error: {err}"),
        }
    }
}

impl std::error::Error for SeedError {}

impl From<sqlx::Error> for SeedError {
    fn from(err: sqlx::Error) -> Self {
        SeedError::Database(err)
    }
}

/// The password every seeded member has, for logging in during
/// development. Seeding refuses to touch a database with real members, so
/// it never reaches a real forum.
pub const DEV_PASSWORD: &str = "tuesday-club";

#[derive(Debug, Default)]
pub struct SeedSummary {
    pub users: usize,
    pub categories: usize,
    pub threads: usize,
    pub posts: usize,
}

/// The highest id in each table that `reset` deletes from, as JSON. Later
/// milestones' tables that hang off users (sessions, invites, …) are
/// emptied by the CASCADE and should be added here too.
const HIGH_WATER_MARK: &str = "jsonb_build_object(
    'users', (SELECT coalesce(max(id), 0) FROM users),
    'sections', (SELECT coalesce(max(id), 0) FROM sections),
    'categories', (SELECT coalesce(max(id), 0) FROM categories),
    'threads', (SELECT coalesce(max(id), 0) FROM threads),
    'posts', (SELECT coalesce(max(id), 0) FROM posts))";

/// Deletes all users and forum content, but only from a database that
/// [`seed`] filled and nobody has added to since (or an empty one).
/// Settings other than the seed's own record are kept.
pub async fn reset(db: &PgPool) -> Result<(), SeedError> {
    let mut tx = db.begin().await?;
    sqlx::raw_sql("LOCK TABLE users, sections, categories, threads, posts IN EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let (empty, matches): (bool, Option<bool>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT NOT EXISTS (SELECT 1 FROM users) AND NOT EXISTS (SELECT 1 FROM sections)
                AND NOT EXISTS (SELECT 1 FROM categories),
                (SELECT value = {HIGH_WATER_MARK} FROM settings WHERE key = 'dev_seed')"
    )))
    .fetch_one(&mut *tx)
    .await?;
    match (empty, matches) {
        (true, _) | (_, Some(true)) => {}
        (false, None) => return Err(SeedError::NotSeeded),
        (false, Some(false)) => return Err(SeedError::ChangedSinceSeed),
    }
    sqlx::raw_sql(
        "TRUNCATE posts, threads, categories, sections, users RESTART IDENTITY CASCADE;
         DELETE FROM settings WHERE key = 'dev_seed';",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Fills an empty database with sample content, all in one transaction.
pub async fn seed(db: &PgPool) -> Result<SeedSummary, SeedError> {
    let mut tx = db.begin().await?;
    let existing: i64 = sqlx::query_scalar!(
        r#"SELECT (SELECT count(*) FROM users) + (SELECT count(*) FROM categories) AS "n!""#
    )
    .fetch_one(&mut *tx)
    .await?;
    if existing > 0 {
        return Err(SeedError::NotEmpty);
    }
    let mut seeder = Seeder {
        db: &mut tx,
        rng: Rng(0x5eed_7e55_da7c_1ab5),
        now: Utc::now()
            .duration_trunc(Duration::minutes(1))
            .expect("minutes divide time"),
        users: Vec::new(),
        summary: SeedSummary::default(),
    };
    seeder.run().await?;
    let summary = seeder.summary;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO settings (key, value) VALUES ('dev_seed', {HIGH_WATER_MARK})
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()"
    )))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(summary)
}

/// xorshift64*: small, fast, and the same everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

struct Seeder<'a> {
    db: &'a mut PgConnection,
    rng: Rng,
    now: DateTime<Utc>,
    /// (username, id)
    users: Vec<(&'static str, i64)>,
    summary: SeedSummary,
}

struct UserSpec {
    username: &'static str,
    display_name: &'static str,
    role: &'static str,
    title: Option<&'static str>,
    pronouns: Option<&'static str>,
    status_line: Option<&'static str>,
    location: Option<&'static str>,
    invited_by: Option<&'static str>,
    joined_days_ago: i64,
}

const USERS: &[UserSpec] = &[
    UserSpec {
        username: "sam",
        display_name: "Sam",
        role: "admin",
        title: Some("Organiser"),
        pronouns: Some("they/them"),
        status_line: Some("Keeper of the spreadsheet"),
        location: Some("Sheffield"),
        invited_by: None,
        joined_days_ago: 640,
    },
    UserSpec {
        username: "ruth",
        display_name: "Ruth",
        role: "moderator",
        title: Some("Book club host"),
        pronouns: Some("she/her"),
        status_line: Some("Reading: Le Guin, again"),
        location: Some("Leeds"),
        invited_by: Some("sam"),
        joined_days_ago: 630,
    },
    UserSpec {
        username: "jonah",
        display_name: "Jonah",
        role: "user",
        title: Some("Rules lawyer"),
        pronouns: Some("he/him"),
        status_line: Some("Learning Spirit Island the hard way"),
        location: None,
        invited_by: Some("sam"),
        joined_days_ago: 570,
    },
    UserSpec {
        username: "maya",
        display_name: "Maya",
        role: "user",
        title: Some("Solo gamer"),
        pronouns: Some("she/her"),
        status_line: Some("Mostly solo, mostly Arkham"),
        location: Some("York"),
        invited_by: Some("ruth"),
        joined_days_ago: 620,
    },
    UserSpec {
        username: "ollie",
        display_name: "Ollie",
        role: "user",
        title: Some("Contrarian"),
        pronouns: Some("he/him"),
        status_line: Some("Wrong on the internet since 2003"),
        location: None,
        invited_by: Some("jonah"),
        joined_days_ago: 590,
    },
    UserSpec {
        username: "kit",
        display_name: "Kit",
        role: "user",
        title: Some("Paint-stained"),
        pronouns: Some("they/them"),
        status_line: None,
        location: Some("Bradford"),
        invited_by: Some("maya"),
        joined_days_ago: 460,
    },
    UserSpec {
        username: "dev",
        display_name: "Dev",
        role: "user",
        title: Some("Tuesday regular"),
        pronouns: Some("he/they"),
        status_line: Some("Will teach anything once"),
        location: None,
        invited_by: Some("sam"),
        joined_days_ago: 420,
    },
    UserSpec {
        username: "priya",
        display_name: "Priya",
        role: "user",
        title: Some("Snack correspondent"),
        pronouns: Some("she/her"),
        status_line: Some("There is always room for a fourth biscuit"),
        location: Some("Harrogate"),
        invited_by: Some("ruth"),
        joined_days_ago: 400,
    },
    UserSpec {
        username: "lin",
        display_name: "Lin",
        role: "user",
        title: None,
        pronouns: Some("she/her"),
        status_line: None,
        location: None,
        invited_by: Some("dev"),
        joined_days_ago: 300,
    },
    UserSpec {
        username: "felix",
        display_name: "Felix",
        role: "user",
        title: Some("Kickstarter survivor"),
        pronouns: Some("he/him"),
        status_line: Some("Still waiting for a 2023 delivery"),
        location: Some("Wakefield"),
        invited_by: Some("ollie"),
        joined_days_ago: 280,
    },
    UserSpec {
        username: "tomasz",
        display_name: "Tomasz",
        role: "user",
        title: Some("New in town"),
        pronouns: Some("he/him"),
        status_line: None,
        location: Some("Leeds"),
        invited_by: Some("kit"),
        joined_days_ago: 40,
    },
    UserSpec {
        username: "ana",
        display_name: "Ana",
        role: "user",
        title: Some("Shelf curator"),
        pronouns: None,
        status_line: Some("One in, one out (in theory)"),
        location: None,
        invited_by: Some("priya"),
        joined_days_ago: 200,
    },
    UserSpec {
        username: "bea",
        display_name: "Bea",
        role: "user",
        title: Some("Knits during rules explanations"),
        pronouns: Some("she/they"),
        status_line: None,
        location: Some("Otley"),
        invited_by: Some("kit"),
        joined_days_ago: 150,
    },
    UserSpec {
        username: "hugo",
        display_name: "Hugo",
        role: "user",
        title: None,
        pronouns: None,
        status_line: Some("Lurker, reformed"),
        location: None,
        invited_by: Some("lin"),
        joined_days_ago: 90,
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Topic {
    Announcements,
    Chat,
    Cooking,
    Intros,
    Games,
    Books,
    Making,
    Lounge,
    Organising,
}

impl Topic {
    fn sentences(self) -> &'static [&'static str] {
        match self {
            Topic::Announcements => &[
                "Doors open at seven, as usual, and the back room is ours until eleven.",
                "Please sign up on the sheet so we know how many tables to ask for.",
                "If you’re bringing a guest, let one of the organisers know beforehand.",
                "The library of club games now lives in the cupboard by the bar.",
                "Thanks to everyone who helped tidy up last week.",
                "We’ll review how this is working at the end of the month.",
            ],
            Topic::Chat | Topic::Lounge => &[
                "I think the answer is probably ‘it depends’, which is the least satisfying answer.",
                "This came up at the pub last week and nobody could agree.",
                "Honestly I’ve changed my mind on this about three times this year.",
                "My partner has opinions on this and I am not allowed to share them.",
                "Is it just me, or has this autumn gone very quickly?",
                "I tried this once and I’d do it again, with reservations.",
                "The trick is to not overthink it, which I am constitutionally unable to do.",
                "Someone at work recommended this and I’ve been hooked since.",
                "I’ll report back once I’ve tried it properly.",
                "Ten out of ten, would argue about again.",
            ],
            Topic::Cooking => &[
                "Made a huge pot of dal on Sunday and I’ve been eating it all week.",
                "Roast squash with miso butter is the best thing I’ve cooked this month.",
                "Tried the no-knead bread recipe everyone talks about. It works.",
                "I’ve given up on meal planning and embraced the chaos.",
                "Leek and potato soup, because it’s that time of year.",
                "My sourdough starter is called Gerald and he is thriving.",
                "Batch-cooked chilli for the week, with far too much cumin.",
                "Does anyone have a good recipe for parkin? Bonfire night is coming.",
                "Pasta e fagioli, which is just beans and pasta pretending to be fancy.",
                "Burned the onions, started again, regretted nothing.",
            ],
            Topic::Intros => &[
                "I moved here in the summer and a friend suggested I come along.",
                "Mostly into cooperative games, but happy to be talked into anything.",
                "I used to play a lot at university and I’m trying to get back into it.",
                "I’ll be at the next Tuesday meetup, probably looking slightly lost.",
                "Welcome! The first meetup is the hardest, it gets easier.",
                "You’ll fit right in. Bring snacks and nobody will ask questions.",
                "Who invited you? We like to keep track of the family tree.",
            ],
            Topic::Games => &[
                "Finally got Spirit Island to the table with the Jagged Earth spirits.",
                "We lost on turn six to a blight cascade and I immediately wanted to play again.",
                "Cascadia twice with my partner, and the landmark variant makes the end game much tighter.",
                "Heat: Pedal to the Metal with five took ten minutes to teach and forty to play.",
                "Brass: Birmingham is still the best game I own, and I will not be taking questions.",
                "Three games of Arkham Horror LCG and one very long game of Mage Knight.",
                "The teach is the hard part; the game itself is fairly simple once it clicks.",
                "I think the two-player variant is better than the full game, honestly.",
                "We house-ruled it after the first game and never looked back.",
                "It’s a great game that I never want to play with more than three people.",
                "The insert is terrible, so everything is now in sandwich bags.",
                "Wingspan’s ‘when played’ powers trigger once, so tucking doesn’t repeat them.",
                "I’d rather play a short game twice than a long game once.",
                "Is it worth it at two players? I keep going back and forth.",
            ],
            Topic::Books => &[
                "Genly Ai is such a frustrating narrator, which I think is the point.",
                "I read it in two sittings and then had to lie down.",
                "The ice crossing chapters are some of the best writing I’ve read this year.",
                "Piranesi is a book about loneliness pretending to be a book about a house.",
                "I bounced off it the first time and loved it the second time.",
                "Can we do something shorter next month? Some of us have jobs.",
                "Currently halfway through and refusing to read anyone’s comments yet.",
                "I keep a list of books people here recommend and it is now very long.",
                "The library had a waiting list of twelve, so I caved and bought it.",
                "Rereading old favourites counts as reading, I will hear no argument.",
            ],
            Topic::Making => &[
                "Painted my first batch of miniatures and they look fine from a distance.",
                "Contrast paints are cheating and I fully endorse cheating.",
                "Printed a new insert for Root and it actually fits this time.",
                "Thin your paints, they said. They were right, annoyingly.",
                "The trick with knitting during a rules explanation is to pick a simple pattern.",
                "Varnish everything. Learn from my mistakes.",
                "My desk is now about seventy percent paint pots.",
                "I sanded the edges for an hour and it was weirdly relaxing.",
            ],
            Topic::Organising => &[
                "The venue have put the room hire up by ten percent from January.",
                "I’ve pencilled in the second Saturday of December for the social.",
                "We have a small surplus from the summer, so we could subsidise the social.",
                "Let’s agree the budget at the next organisers’ call.",
                "I can sort the booking if someone else handles the sign-up sheet.",
            ],
        }
    }
}

const REACTIONS: &[&str] = &[
    "Completely agree.",
    "Ha, yes.",
    "This is the correct take.",
    "I have to disagree, gently.",
    "Seconded!",
    "Ooh, adding that to the list.",
    "Same here, more or less.",
    "Fair point, I hadn’t thought of it that way.",
];

/// The general chat titles, for a category with several pages of threads.
const CHAT_TITLES: &[&str] = &[
    "Favourite rainy-day pub in Leeds?",
    "What podcasts are you listening to?",
    "Desk setups: show us yours",
    "Anyone else doing the couch to 5k?",
    "Allotment update thread",
    "Cats of the Tuesday Club",
    "Dogs of the Tuesday Club",
    "Best biscuit for dunking (serious answers only)",
    "Train strikes and getting to meetups",
    "What are you watching at the moment?",
    "Recommend me a coffee grinder",
    "Small wins this week",
    "Christmas market: worth the crowds?",
    "The great tea-versus-coffee schism",
    "Weekend walks in the Dales",
    "Has anyone tried the new Korean place on Call Lane?",
    "Puzzles: jigsaws, crosswords, cryptics",
    "Learning a language as an adult",
    "Help me name my sourdough starter",
    "Bike commuting in winter",
    "Song stuck in your head right now",
    "Photography thread: autumn colours",
    "Plants that survive neglect",
    "Retro games that hold up",
    "Things you changed your mind about",
    "Moving house: survival tips",
    "Favourite museum in the north",
    "Music for concentrating",
    "Half-term ideas for kids who like games",
    "The oddest thing in your fridge right now",
    "Charity shop finds",
    "Learning to juggle (badly)",
    "Pens, notebooks and other stationery joys",
    "Is it too early for mince pies?",
    "Local gig recommendations",
    "Kitchen gadgets worth the cupboard space",
    "Favourite walking boots",
    "What did you learn this week?",
];

struct ThreadSpec {
    title: &'static str,
    author: &'static str,
    posts: usize,
    days_ago: i64,
    pinned: bool,
    locked: bool,
    /// How long ago the last generated post was.
    quiet_hours: i64,
}

const fn thread(
    title: &'static str,
    author: &'static str,
    posts: usize,
    days_ago: i64,
) -> ThreadSpec {
    ThreadSpec {
        title,
        author,
        posts,
        days_ago,
        pinned: false,
        locked: false,
        quiet_hours: 0,
    }
}

const fn pinned(spec: ThreadSpec, locked: bool) -> ThreadSpec {
    ThreadSpec {
        pinned: true,
        locked,
        ..spec
    }
}

impl Seeder<'_> {
    async fn run(&mut self) -> Result<(), SeedError> {
        sqlx::query!(
            "INSERT INTO settings (key, value) VALUES ('site_name', to_jsonb('The Tuesday Club'::text))
             ON CONFLICT (key) DO NOTHING"
        )
        .execute(&mut *self.db)
        .await?;

        let password_hash = crate::accounts::hash_password(DEV_PASSWORD).await;
        for spec in USERS {
            let invited_by = spec.invited_by.map(|name| self.user(name));
            let id = store::create_user(
                &mut *self.db,
                &NewUser {
                    username: spec.username.into(),
                    display_name: spec.display_name.into(),
                    email: format!("{}@tuesday.example", spec.username),
                    role: Some(spec.role.into()),
                    title: spec.title.map(Into::into),
                    pronouns: spec.pronouns.map(Into::into),
                    status_line: spec.status_line.map(Into::into),
                    location: spec.location.map(Into::into),
                    invited_by_user_id: invited_by,
                    created_at: Some(self.now - Duration::days(spec.joined_days_ago)),
                },
            )
            .await?;
            sqlx::query!(
                "UPDATE users SET password_hash = $2, email_verified_at = created_at WHERE id = $1",
                id,
                password_hash
            )
            .execute(&mut *self.db)
            .await?;
            self.users.push((spec.username, id));
        }
        self.summary.users = self.users.len();

        let community = store::create_section(&mut *self.db, "Community", 1).await?;
        let hobbies = store::create_section(&mut *self.db, "Hobbies", 2).await?;
        let members = store::create_section(&mut *self.db, "Members only", 3).await?;

        let announcements = self
            .category(
                community,
                1,
                "announcements",
                "Announcements",
                "News about the club and this forum. Only organisers can start threads.",
                "public",
                "staff",
            )
            .await?;
        let general = self
            .category(
                community,
                2,
                "general",
                "General chat",
                "Anything that doesn’t fit elsewhere.",
                "public",
                "members",
            )
            .await?;
        let introductions = self
            .category(
                community,
                3,
                "introductions",
                "Introductions",
                "New here? Say hello, and tell us who invited you.",
                "public",
                "members",
            )
            .await?;
        let games = self
            .category(
                hobbies,
                1,
                "board-games",
                "Board games",
                "What you played, rules questions, recommendations.",
                "public",
                "members",
            )
            .await?;
        let books = self
            .category(
                hobbies,
                2,
                "books",
                "Books & reading",
                "Book club threads and recommendations.",
                "public",
                "members",
            )
            .await?;
        let making = self
            .category(
                hobbies,
                3,
                "making-things",
                "Making things",
                "Crafts, 3D printing, woodwork, painting miniatures.",
                "public",
                "members",
            )
            .await?;
        let lounge = self
            .category(
                members,
                1,
                "lounge",
                "Members’ lounge",
                "Off-the-record chat. Visible only to logged-in members.",
                "members",
                "members",
            )
            .await?;
        let organisers = self
            .category(
                members,
                2,
                "organisers",
                "Organisers",
                "Planning events and running the club.",
                "groups",
                "groups",
            )
            .await?;

        // Announcements
        self.thread(
            announcements,
            Topic::Announcements,
            pinned(thread("Forum rules, version 2", "sam", 1, 320), true),
            Some(RULES),
        )
        .await?;
        self.thread(
            announcements,
            Topic::Announcements,
            thread("Tuesday meetups: autumn schedule", "sam", 9, 34),
            None,
        )
        .await?;
        self.thread(
            announcements,
            Topic::Chat,
            thread("Welcome to the new forum", "sam", 14, 300),
            None,
        )
        .await?;

        // General chat: one very long thread and enough others for paging.
        self.thread(
            general,
            Topic::Cooking,
            thread("What are you cooking this week?", "priya", 163, 200),
            None,
        )
        .await?;
        for (i, title) in CHAT_TITLES.iter().enumerate() {
            let author = self.rng.pick(USERS).username;
            let posts = 1 + self.rng.below(24);
            self.thread(
                general,
                Topic::Chat,
                thread(title, author, posts, 5 + 7 * i as i64),
                None,
            )
            .await?;
        }

        // Introductions
        for (title, author, days) in [
            ("Hi from Leeds!", "tomasz", 38),
            ("Hello from a lapsed D&D player", "hugo", 88),
            ("Bea here, knitting and co-op games", "bea", 148),
            ("New member: Ana", "ana", 198),
            ("Felix says hi (and apologises in advance)", "felix", 278),
        ] {
            let posts = 4 + self.rng.below(8);
            self.thread(
                introductions,
                Topic::Intros,
                thread(title, author, posts, days),
                None,
            )
            .await?;
        }

        // Board games
        self.thread(
            games,
            Topic::Games,
            pinned(
                thread("How this category works (read me)", "sam", 1, 270),
                true,
            ),
            Some(GAMES_README),
        )
        .await?;
        let september = self.september_thread(games).await?;
        self.solo_fork(games, september).await?;
        for spec in [
            thread(
                "Spirit Island: is Jagged Earth worth it if you only play two-player?",
                "jonah",
                32,
                4,
            ),
            thread(
                "Looking for a fourth for Tuesday 7th (Brass: Birmingham)",
                "dev",
                7,
                1,
            ),
            thread(
                "Teaching heavy games to new players without losing them",
                "maya",
                59,
                10,
            ),
            thread("Cascadia scoring variants we actually enjoy", "kit", 15, 6),
            thread(
                "Sleeving cards: sensible protection or snobbery?",
                "tomasz",
                45,
                13,
            ),
            thread(
                "Storage for big-box games without a dedicated room",
                "ana",
                28,
                19,
            ),
            thread(
                "Rules question: Wingspan “when played” powers and tucking",
                "lin",
                10,
                5,
            ),
            thread(
                "Crowdfunding fatigue: anyone else stopped backing games?",
                "felix",
                74,
                29,
            ),
            thread(
                "Two-player games that aren’t just cut-down four-player games",
                "priya",
                40,
                32,
            ),
        ] {
            self.thread(games, Topic::Games, spec, None).await?;
        }

        // Books
        self.thread(
            books,
            Topic::Books,
            pinned(
                thread("October read: The Left Hand of Darkness", "ruth", 70, 12),
                false,
            ),
            None,
        )
        .await?;
        self.thread(
            books,
            Topic::Books,
            thread("What are you reading right now?", "ruth", 124, 240),
            None,
        )
        .await?;
        self.thread(
            books,
            Topic::Books,
            thread("September read: Piranesi", "ruth", 48, 42),
            None,
        )
        .await?;
        self.thread(
            books,
            Topic::Books,
            thread("Comfort rereads", "ana", 22, 60),
            None,
        )
        .await?;

        // Making things
        self.thread(
            making,
            Topic::Making,
            thread("Painted my first miniatures (be kind)", "kit", 18, 3),
            None,
        )
        .await?;
        self.thread(
            making,
            Topic::Making,
            thread("3D-printed inserts: worth it?", "dev", 25, 25),
            None,
        )
        .await?;
        self.thread(
            making,
            Topic::Making,
            thread(
                "Knitting while listening to rules explanations",
                "bea",
                9,
                50,
            ),
            None,
        )
        .await?;

        // Members only
        self.thread(
            lounge,
            Topic::Lounge,
            thread("Venue for the December social", "jonah", 30, 9),
            None,
        )
        .await?;
        self.thread(
            lounge,
            Topic::Lounge,
            thread("Off the record: the great snack debate", "priya", 15, 70),
            None,
        )
        .await?;
        self.thread(
            organisers,
            Topic::Organising,
            thread("Budget for 2027", "sam", 12, 8),
            None,
        )
        .await?;

        Ok(())
    }

    fn user(&self, username: &str) -> i64 {
        self.users
            .iter()
            .find(|(name, _)| *name == username)
            .map(|(_, id)| *id)
            .unwrap_or_else(|| panic!("no seed user {username}"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn category(
        &mut self,
        section_id: i64,
        position: i32,
        slug: &str,
        name: &str,
        description: &str,
        read_level: &str,
        write_level: &str,
    ) -> sqlx::Result<i64> {
        self.summary.categories += 1;
        store::create_category(
            &mut *self.db,
            &NewCategory {
                section_id: Some(section_id),
                slug: slug.into(),
                name: name.into(),
                description: description.into(),
                position,
                read_level: read_level.into(),
                write_level: write_level.into(),
            },
        )
        .await
    }

    fn paragraph(&mut self, topic: Topic) -> String {
        let sentences = topic.sentences();
        let count = 1 + self.rng.below(3);
        (0..count)
            .map(|_| *self.rng.pick(sentences))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn body(&mut self, topic: Topic) -> String {
        let mut paragraphs = Vec::new();
        if self.rng.chance(25) {
            paragraphs.push(self.rng.pick(REACTIONS).to_string());
        }
        for _ in 0..1 + self.rng.below(2) {
            paragraphs.push(self.paragraph(topic));
        }
        if self.rng.chance(8) {
            let items: Vec<String> = (0..2 + self.rng.below(3))
                .map(|_| format!("- {}", self.rng.pick(topic.sentences())))
                .collect();
            paragraphs.push(items.join("\n"));
        }
        paragraphs.join("\n\n")
    }

    /// Times for `count` posts, oldest first, starting `days_ago` days ago
    /// and ending no later than `quiet_hours` ago.
    fn timeline(&mut self, days_ago: i64, quiet_hours: i64, count: usize) -> Vec<DateTime<Utc>> {
        let start = self.now - Duration::days(days_ago);
        let end = self.now - Duration::hours(quiet_hours);
        let span = (end - start).num_minutes().max(1);
        let mut offsets: Vec<i64> = (0..count)
            .map(|_| (self.rng.next() % span as u64) as i64)
            .collect();
        offsets.sort();
        offsets[0] = 0;
        offsets
            .into_iter()
            .map(|m| start + Duration::minutes(m))
            .collect()
    }

    async fn thread(
        &mut self,
        category_id: i64,
        topic: Topic,
        spec: ThreadSpec,
        opening: Option<&str>,
    ) -> sqlx::Result<SeededThread> {
        let times = self.timeline(spec.days_ago, spec.quiet_hours, spec.posts);
        let body = match opening {
            Some(text) => text.to_owned(),
            None => self.body(topic),
        };
        let author_id = self.user(spec.author);
        let created = store::create_thread(
            &mut *self.db,
            &NewThread {
                category_id,
                title: spec.title.into(),
                author_id,
                body_md: body.clone(),
                forked_from_post_id: None,
                pinned: spec.pinned,
                locked: spec.locked,
                created_at: Some(times[0]),
            },
        )
        .await?;
        self.summary.threads += 1;
        self.summary.posts += 1;
        let mut thread = SeededThread {
            created,
            posts: vec![(created.first_post_id, spec.author, first_sentence(&body))],
            forkable: None,
        };
        for at in &times[1..] {
            let author = self.rng.pick(USERS).username;
            self.reply(&mut thread, topic, author, *at).await?;
        }
        Ok(thread)
    }

    /// A reply, sometimes quoting a recent post in the thread.
    async fn reply(
        &mut self,
        thread: &mut SeededThread,
        topic: Topic,
        author: &'static str,
        at: DateTime<Utc>,
    ) -> sqlx::Result<i64> {
        let mut body = self.body(topic);
        let mut reply_to = None;
        if self.rng.chance(25) {
            let recent = thread.posts.len().saturating_sub(8);
            let (id, _, quoted) =
                &thread.posts[recent + self.rng.below(thread.posts.len() - recent)];
            body = format!("> {quoted}\n\n{body}");
            reply_to = Some(*id);
        }
        self.post(thread, author, body, reply_to, at).await
    }

    async fn post(
        &mut self,
        thread: &mut SeededThread,
        author: &'static str,
        body: String,
        reply_to: Option<i64>,
        at: DateTime<Utc>,
    ) -> sqlx::Result<i64> {
        let author_id = self.user(author);
        let id = store::create_post(
            &mut *self.db,
            &NewPost {
                thread_id: thread.created.thread_id,
                author_id,
                body_md: body.clone(),
                reply_to_post_id: reply_to,
                created_at: Some(at),
            },
        )
        .await?;
        self.summary.posts += 1;
        thread.posts.push((id, author, first_sentence(&body)));
        Ok(id)
    }

    /// The thread from the mockups: 107 posts, ending with a quote-reply,
    /// an edited post that gets quote-forked, and a deleted post.
    async fn september_thread(&mut self, games: i64) -> sqlx::Result<SeededThread> {
        let mut thread = self
            .thread(
                games,
                Topic::Games,
                ThreadSpec {
                    quiet_hours: 22,
                    ..pinned(thread("What did you play this week? (September)", "sam", 100, 30), false)
                },
                Some("Same as every month: what got played, what you thought, and whether it’s staying on the shelf."),
            )
            .await?;
        let day = |seeder: &Self, hours_ago: i64| seeder.now - Duration::hours(hours_ago);
        self.post(&mut thread, "jonah", "Finally got Spirit Island to the table with the Jagged Earth spirits. Played Finder of Paths Unseen, which is every bit as confusing as people warned.\n\nWe lost on turn six to a blight cascade and I immediately wanted to play again.".into(), None, day(self, 20))
            .await?;
        let jonah_post = thread.posts.last().expect("just posted").0;
        self.post(&mut thread, "ruth", "> We lost on turn six to a blight cascade\n\nThis is the Spirit Island experience in one sentence. Finder is the spirit I’d least recommend for a first Jagged Earth game. Try Lure of the Deep Wilderness next.".into(), Some(jonah_post), day(self, 19))
            .await?;
        self.post(&mut thread, "kit", "Cascadia twice with my partner. We’ve started using the landmark variant from the extras box and it makes the end game much tighter.".into(), None, day(self, 8))
            .await?;
        let maya_post = self
            .post(&mut thread, "maya", "Mostly solo this week: three games of Arkham Horror LCG and one very long game of Mage Knight.\n\nI’m starting to think solo play is its own hobby that happens to share equipment with this one.".into(), None, day(self, 3))
            .await?;
        sqlx::query!(
            "UPDATE posts SET edited_at = created_at + interval '8 minutes' WHERE id = $1",
            maya_post
        )
        .execute(&mut *self.db)
        .await?;
        self.post(&mut thread, "ollie", "> solo play is its own hobby that happens to share equipment with this one\n\nStrong disagree, but this deserves its own thread rather than derailing this one. Starting one now.".into(), Some(maya_post), day(self, 3) + Duration::minutes(16))
            .await?;
        let deleted = self
            .post(
                &mut thread,
                "ollie",
                "Actually, never mind.".into(),
                None,
                day(self, 2),
            )
            .await?;
        sqlx::query!(
            "UPDATE posts SET deleted_at = created_at + interval '1 minute' WHERE id = $1",
            deleted
        )
        .execute(&mut *self.db)
        .await?;
        self.post(&mut thread, "dev", "Tuesday group played Heat: Pedal to the Metal with five. Teaching took ten minutes and the race took forty. Best new-to-us game of the year so far.".into(), None, day(self, 1))
            .await?;
        thread.forkable = Some(maya_post);
        Ok(thread)
    }

    async fn solo_fork(&mut self, games: i64, september: SeededThread) -> sqlx::Result<()> {
        let source = september
            .forkable
            .expect("september thread has a post to fork");
        let start = self.now - Duration::hours(3) + Duration::minutes(18);
        let author_id = self.user("ollie");
        let created = store::create_thread(
            &mut *self.db,
            &NewThread {
                category_id: games,
                title: "Do solo modes count as ‘playing’ a game?".into(),
                author_id,
                body_md: "Strong disagree, but it deserves its own thread rather than derailing that one. Is a solo game a different hobby, or the same hobby with fewer snacks?".into(),
                forked_from_post_id: Some(source),
                pinned: false,
                locked: false,
                created_at: Some(start),
            },
        )
        .await?;
        self.summary.threads += 1;
        self.summary.posts += 1;
        let mut thread = SeededThread {
            created,
            posts: vec![(
                created.first_post_id,
                "ollie",
                "Is a solo game a different hobby?".into(),
            )],
            forkable: None,
        };
        let span = (self.now - start).num_minutes();
        for i in 1..23 {
            let author = self.rng.pick(USERS).username;
            self.reply(
                &mut thread,
                Topic::Games,
                author,
                start + Duration::minutes(span * i / 23),
            )
            .await?;
        }
        // One reply in the fork broke the rules and was hidden.
        let hidden = thread.posts[5].0;
        sqlx::query!("UPDATE posts SET hidden_at = now() WHERE id = $1", hidden)
            .execute(&mut *self.db)
            .await?;
        // And one post was edited after the fact.
        let edited = thread.posts[2].0;
        let body = "Edited to add: I take it back, solo games count. Mostly.";
        sqlx::query!(
            "UPDATE posts SET body_md = $2, body_html = $3, edited_at = created_at + interval '2 hours' WHERE id = $1",
            edited,
            body,
            markdown::render(body)
        )
        .execute(&mut *self.db)
        .await?;
        Ok(())
    }
}

struct SeededThread {
    created: CreatedThread,
    /// (id, author, a sentence to quote)
    posts: Vec<(i64, &'static str, String)>,
    forkable: Option<i64>,
}

/// The first sentence of a post's first plain paragraph, for quoting.
fn first_sentence(body: &str) -> String {
    let paragraph = body
        .split("\n\n")
        .find(|p| !p.starts_with('>') && !p.starts_with('-'))
        .unwrap_or(body);
    let end = paragraph.find(". ").map_or(paragraph.len(), |i| i + 1);
    paragraph[..end].trim().to_owned()
}

const RULES: &str = "Welcome to the Tuesday Club forum. The short version:

1. **Be kind.** Disagree with ideas, not people.
2. **No spoilers** in thread titles. Use the book club threads for discussion.
3. **Keep it legal and keep it friendly.** Moderators may hide posts that aren’t.
4. **Invites are personal.** You’re responsible for the people you invite.

If something’s wrong, use the report link on the post, or message any of the organisers.

| Who | What they do |
|---|---|
| Sam | Organiser, admin |
| Ruth | Book club host, moderator |

Thanks for being here.";

const GAMES_README: &str = "This is the place for anything to do with board and card games.

- **Weekly threads** (‘What did you play this week?’) are pinned. Post there rather than starting a new thread for every game night.
- **Rules questions** get their own threads. Put the game’s name first in the title.
- **Looking for players?** Start a thread with the date and the game.

Want to split a tangent off into its own discussion? Use *quote in new thread* on the post that started it.";
