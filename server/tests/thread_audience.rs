//! A thread's audience page and indicators (DESIGN.md §3.4, milestone 14):
//! `/t/{id}/audience` says who can read and reply in a thread, naming
//! lists only as §3.3 allows, and gives the thread's author and its
//! space's administrators a form to set, change or lift its restriction;
//! a thread narrower than its space shows a lock and its audience by its
//! title, beside the reply box and in thread lists; and a new thread can
//! be restricted from the start.

mod common;

use axum::http::StatusCode;
use common::Forum;
use parlour::changes::Change;
use parlour::permissions::{Grant, ThreadRestriction};
use parlour::store;
use sqlx::PgPool;

struct Club {
    f: Forum,
    ruth: i64,
    jonah: i64,
    maya: i64,
    kit: i64,
    /// Ruth's Friends list, which Jonah is on.
    friends: i64,
    /// A site group, which Kit is in.
    organisers: i64,
    general: i64,
    ruth_cookie: String,
    jonah_cookie: String,
    maya_cookie: String,
    kit_cookie: String,
    staff_cookie: String,
}

/// Ruth, Jonah, Maya and Kit are members, and Sam a moderator. Jonah is on
/// Ruth's Friends list, and Kit is in the Organisers group. General is a
/// public space every member may post in.
async fn club(pool: PgPool) -> Club {
    let f = Forum::new(pool);
    f.no_new_members().await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let maya = f.member("maya", "user", None).await;
    let kit = f.member("kit", "user", None).await;
    let sam = f.member("sam", "moderator", None).await;
    let friends = f
        .id(&format!(
            "SELECT id FROM groups WHERE owner_user_id = {ruth} AND name = 'Friends'"
        ))
        .await;
    f.join_group(friends, jonah).await;
    let organisers = f.group("organisers", "Organisers").await;
    f.join_group(organisers, kit).await;
    let general = f.space("general", "public", None, 0).await;
    Club {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        maya_cookie: f.session(maya).await,
        kit_cookie: f.session(kit).await,
        staff_cookie: f.session(sam).await,
        f,
        ruth,
        jonah,
        maya,
        kit,
        friends,
        organisers,
        general,
    }
}

impl Club {
    /// Gives a thread its own restriction directly, as the store does, and
    /// clears the cached pages as the handler would.
    async fn restrict(&self, thread: i64, read: Option<Grant>, reply: Option<Grant>) {
        assert!(
            store::set_thread_restriction(&self.f.db, thread, &ThreadRestriction { read, reply })
                .await
                .unwrap()
        );
        self.f
            .changed(Change::ThreadChanged {
                thread_id: thread,
                previous_space_id: None,
            })
            .await;
    }

    async fn visibility(&self, thread: i64) -> String {
        sqlx::query_scalar("SELECT visibility FROM threads WHERE id = $1")
            .bind(thread)
            .fetch_one(&self.f.db)
            .await
            .unwrap()
    }

    /// The thread's restriction as stored: each part's group and user ids.
    async fn stored(&self, thread: i64) -> StoredRestriction {
        sqlx::query_as(
            "SELECT read_group_ids, read_user_ids, reply_group_ids, reply_user_ids
             FROM threads WHERE id = $1",
        )
        .bind(thread)
        .fetch_one(&self.f.db)
        .await
        .unwrap()
    }
}

type StoredRestriction = (
    Option<Vec<i64>>,
    Option<Vec<i64>>,
    Option<Vec<i64>>,
    Option<Vec<i64>>,
);

fn audience(thread: i64) -> String {
    format!("/t/{thread}/audience")
}

fn form_action(thread: i64) -> String {
    format!("action=\"/t/{thread}/audience\"")
}

#[sqlx::test]
async fn the_audience_page_says_who_can_read_and_reply(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let open = f
        .thread(club.general, club.maya, "Open house", "Come one, come all.")
        .await
        .thread_id;
    let plans = f
        .thread(club.general, club.ruth, "Secret plans", "Only for some.")
        .await
        .thread_id;
    club.restrict(
        plans,
        Some(Grant::listed(
            [club.friends, club.organisers],
            [club.ruth, club.maya],
        )),
        Some(Grant::listed([], [club.ruth, club.maya])),
    )
    .await;

    // A thread that inherits says so, and offers its author the form.
    let page = f.page_as(&audience(open), &club.maya_cookie).await;
    assert!(page.contains("Who can take part in Open house"), "{page}");
    assert!(page.contains("Everyone who can read General"), "{page}");
    assert!(page.contains("Everyone who can reply in General"), "{page}");
    assert!(page.contains("href=\"/c/general/audience\""), "{page}");
    assert!(page.contains(&form_action(open)), "{page}");
    // Anyone else who can read it sees who can, without the form.
    let page = f.page_as(&audience(open), &club.jonah_cookie).await;
    assert!(page.contains("Everyone who can read General"), "{page}");
    assert!(!page.contains(&form_action(open)), "{page}");

    // Ruth, whose list it is, sees its name and who is on it.
    let page = f.page_as(&audience(plans), &club.ruth_cookie).await;
    assert!(page.contains("Who can take part in Secret plans"), "{page}");
    assert!(page.contains("Ruth’s Friends"), "{page}");
    assert!(page.contains("Organisers group"), "{page}");
    assert!(page.contains("@jonah"), "{page}");
    assert!(page.contains("@maya"), "{page}");
    assert!(page.contains(&form_action(plans)), "{page}");
    // Jonah, who is on it, sees the same, without the form.
    let page = f.page_as(&audience(plans), &club.jonah_cookie).await;
    assert!(page.contains("Ruth’s Friends"), "{page}");
    assert!(!page.contains(&form_action(plans)), "{page}");
    // Kit, who reads it through the Organisers, sees who is in that, and
    // only whose choice the list is.
    let page = f.page_as(&audience(plans), &club.kit_cookie).await;
    assert!(page.contains("People Ruth has chosen"), "{page}");
    assert!(!page.contains("Friends"), "{page}");
    assert!(!page.contains("@jonah"), "{page}");
    assert!(page.contains("@kit"), "{page}");
    // Maya is named; replying is for Ruth and her.
    let page = f.page_as(&audience(plans), &club.maya_cookie).await;
    assert!(page.contains("Who can reply"), "{page}");
    assert!(
        !page.contains("Everyone who can reply in General"),
        "{page}"
    );

    // Nobody else learns of it, and visitors are asked to log in.
    let maya_out = f
        .thread(club.general, club.ruth, "Without Maya", "Hush.")
        .await
        .thread_id;
    club.restrict(maya_out, Some(Grant::listed([club.friends], [])), None)
        .await;
    let response = f.get_as(&audience(maya_out), Some(&club.maya_cookie)).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert!(!response.body.contains("Without Maya"));
    let response = f.get_as(&audience(open), None).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/login"));
    let response = f
        .get_as("/t/999999/audience", Some(&club.ruth_cookie))
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn a_lock_shows_only_where_a_thread_is_narrower_than_its_space(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let thread = f
        .thread(club.general, club.ruth, "Plans", "Who is coming?")
        .await
        .thread_id;
    let path = format!("/t/{thread}/plans");
    // A thread that inherits shows nothing extra, to anyone.
    for cookie in [None, Some(club.ruth_cookie.as_str())] {
        let page = f.get_as(&path, cookie).await.body;
        assert!(!page.contains("thread-audience"), "{page}");
        let list = f.get_as("/c/general", cookie).await.body;
        assert!(!list.contains("flag-restricted"), "{list}");
    }

    club.restrict(
        thread,
        Some(Grant::listed([club.friends], [club.ruth])),
        None,
    )
    .await;
    // By the title and beside the reply box, with the audience by name.
    let page = f.page_as(&path, &club.ruth_cookie).await;
    assert_eq!(
        page.matches("class=\"thread-audience\"").count(),
        2,
        "{page}"
    );
    assert!(
        page.contains("Only Ruth’s Friends (1) and Ruth can read this thread."),
        "{page}"
    );
    assert!(
        page.contains(&format!("href=\"/t/{thread}/audience\"")),
        "{page}"
    );
    // Someone on the list sees it too; and thread lists show the lock.
    let page = f.page_as(&path, &club.jonah_cookie).await;
    assert!(page.contains("Ruth’s Friends (1)"), "{page}");
    for list_page in ["/c/general", "/latest", "/"] {
        let list = f.page_as(list_page, &club.jonah_cookie).await;
        assert!(list.contains("flag-restricted"), "{list_page}: {list}");
    }
    // Staff can't read it, not being named (DESIGN.md §3.1); under a
    // break-glass grant they read it, and see whose choice it is, not the
    // list.
    assert_eq!(
        f.get_as(&path, Some(&club.staff_cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    parlour::staff_access::open(
        &f.db,
        &parlour::staff_access::NewAccess {
            actor_id: f.user_id("sam").await,
            scope: parlour::staff_access::Scope::Thread(thread),
            report_id: None,
            reason: "Checking",
        },
    )
    .await
    .unwrap();
    let page = f.page_as(&path, &club.staff_cookie).await;
    assert!(page.contains("people Ruth has chosen"), "{page}");
    assert!(!page.contains("Friends"), "{page}");

    // A reply restriction alone narrows only replying: the page says who
    // may, and lists show no lock.
    club.restrict(thread, None, Some(Grant::listed([], [club.ruth])))
        .await;
    let page = f.page_as(&path, &club.maya_cookie).await;
    assert!(page.contains("Only Ruth can reply."), "{page}");
    assert!(!page.contains("can read this thread"), "{page}");
    assert!(page.contains("You can’t reply in this thread."), "{page}");
    let list = f.page_as("/c/general", &club.maya_cookie).await;
    assert!(!list.contains("flag-restricted"), "{list}");
    // And visitors' cached pages say nothing either.
    let page = f.page(&path).await;
    assert!(!page.contains("thread-audience"), "{page}");
}

#[sqlx::test]
async fn a_restriction_no_narrower_than_its_space_shows_no_lock(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    // Only Organisers read, reply and start threads in Organising.
    let organising = f.space("organising", "public", None, 1).await;
    f.restrict(organising, &[club.organisers], Some(&[club.organisers]))
        .await;
    let thread = f
        .thread(organising, club.kit, "Agenda", "What shall we do?")
        .await
        .thread_id;
    let path = format!("/t/{thread}/agenda");
    let organisers = || Grant::listed([club.organisers], [club.kit]);
    // A restriction naming just the space's own groups (and its author)
    // leaves out nobody the space lets in, so it shows nothing extra.
    club.restrict(thread, Some(organisers()), Some(organisers()))
        .await;
    let page = f.page_as(&path, &club.kit_cookie).await;
    assert!(page.contains("Agenda"), "{page}");
    assert!(!page.contains("thread-audience"), "{page}");
    let list = f.page_as("/c/organising", &club.kit_cookie).await;
    assert!(list.contains("Agenda"), "{list}");
    assert!(!list.contains("flag-restricted"), "{list}");

    // Narrower replying alone shows only who may reply.
    club.restrict(
        thread,
        Some(organisers()),
        Some(Grant::listed([], [club.kit])),
    )
    .await;
    let page = f.page_as(&path, &club.kit_cookie).await;
    assert!(page.contains("Only Kit can reply."), "{page}");
    assert!(!page.contains("can read this thread"), "{page}");
    let list = f.page_as("/c/organising", &club.kit_cookie).await;
    assert!(!list.contains("flag-restricted"), "{list}");

    // Narrower reading shows the lock, in the thread and in lists.
    club.restrict(thread, Some(Grant::listed([], [club.kit])), None)
        .await;
    let page = f.page_as(&path, &club.kit_cookie).await;
    assert!(page.contains("Only Kit can read this thread."), "{page}");
    assert!(!page.contains("can reply."), "{page}");
    let list = f.page_as("/c/organising", &club.kit_cookie).await;
    assert!(list.contains("flag-restricted"), "{list}");
}

#[sqlx::test]
async fn the_author_restricts_a_thread_through_the_form(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let thread = f
        .thread(club.general, club.ruth, "Plans", "Who is coming?")
        .await
        .thread_id;
    // Visitors have the thread cached.
    assert!(f.page("/c/general").await.contains("Plans"));
    let friends = club.friends.to_string();
    let response = f
        .submit(
            &audience(thread),
            &[
                ("read", "only"),
                ("read_group", &friends),
                ("read_users", "maya"),
                ("reply", "space"),
            ],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        response.location(),
        format!("/t/{thread}/audience?done=saved")
    );
    assert_eq!(
        club.stored(thread).await,
        (
            Some(vec![club.friends]),
            Some(vec![club.ruth, club.maya]),
            None,
            None
        )
    );
    assert_eq!(club.visibility(thread).await, "restricted");
    let page = f.page_as(response.location(), &club.ruth_cookie).await;
    assert!(page.contains("Saved."), "{page}");
    // The form shows what is saved: the list ticked, Maya by name, and
    // not Ruth, who is always named.
    assert!(page.contains("value=\"maya\""), "{page}");
    assert!(!page.contains("value=\"ruth, maya\""), "{page}");
    // It has gone from visitors' cached pages and from Kit's.
    assert!(!f.page("/c/general").await.contains("Plans"));
    assert!(
        !f.page_as("/c/general", &club.kit_cookie)
            .await
            .contains("Plans")
    );
    assert!(
        f.page_as("/c/general", &club.maya_cookie)
            .await
            .contains("Plans")
    );

    // Only those who can read it may reply: someone named to reply must
    // be named to read, by the same names.
    let response = f
        .submit(
            &audience(thread),
            &[
                ("read", "only"),
                ("read_group", &friends),
                ("reply", "only"),
                ("reply_users", "kit"),
            ],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
    assert!(
        response
            .body
            .contains("Only people who can read the thread can reply in it"),
        "{}",
        response.body
    );
    // Nobody who doesn't exist, and nothing not offered.
    let response = f
        .submit(
            &audience(thread),
            &[
                ("read", "only"),
                ("read_users", "nobody"),
                ("reply", "space"),
            ],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("There is no member called nobody."),
        "{}",
        response.body
    );
    let organisers = club.organisers.to_string();
    let response = f
        .submit(
            &audience(thread),
            &[
                ("read", "only"),
                ("read_group", "123456"),
                ("reply", "space"),
            ],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(club.visibility(thread).await, "restricted");
    // Replying narrowed within reading is fine.
    let response = f
        .submit(
            &audience(thread),
            &[
                ("read", "only"),
                ("read_group", &organisers),
                ("read_users", "maya"),
                ("reply", "only"),
                ("reply_users", "maya"),
            ],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        club.stored(thread).await,
        (
            Some(vec![club.organisers]),
            Some(vec![club.ruth, club.maya]),
            Some(vec![]),
            Some(vec![club.ruth, club.maya])
        )
    );

    // Maya can read it, but it isn't hers to change.
    let response = f
        .submit(
            &audience(thread),
            &[("read", "space"), ("reply", "space")],
            Some(&club.maya_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "{}", response.body);
    // Ruth widens it back to the space's audience: visitors see it again.
    let response = f
        .submit(
            &audience(thread),
            &[("read", "space"), ("reply", "space")],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(club.stored(thread).await, (None, None, None, None));
    assert_eq!(club.visibility(thread).await, "public");
    assert!(f.page("/c/general").await.contains("Plans"));
    // Visitors are asked to log in.
    let response = f
        .submit(&audience(thread), &[("read", "space")], None)
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/login"));
}

#[sqlx::test]
async fn those_who_run_the_space_change_a_thread_they_cant_read(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    f.set_grant(club.general, "administer", Grant::listed([], [club.kit]))
        .await;
    let thread = f
        .thread(club.general, club.ruth, "Secret plans", "Only for some.")
        .await
        .thread_id;
    club.restrict(
        thread,
        Some(Grant::listed([club.friends], [club.ruth])),
        None,
    )
    .await;
    // Kit runs General: he gets the form, but nothing of what the thread
    // says, and the list only as whose choice it is.
    let page = f.page_as(&audience(thread), &club.kit_cookie).await;
    assert!(page.contains(&form_action(thread)), "{page}");
    assert!(!page.contains("Secret plans"), "{page}");
    assert!(!page.contains("Ruth’s Friends"), "{page}");
    assert!(page.contains("People Ruth has chosen"), "{page}");
    // He keeps the list while naming himself too...
    let ticked = f
        .submit(
            &audience(thread),
            &[
                ("read", "only"),
                ("read_group", &format!("owner:{}", club.ruth)),
                ("read_users", "kit"),
                ("reply", "space"),
            ],
            Some(&club.kit_cookie),
        )
        .await;
    assert_eq!(ticked.status, StatusCode::SEE_OTHER, "{}", ticked.body);
    assert_eq!(
        club.stored(thread).await,
        (
            Some(vec![club.friends]),
            Some(vec![club.ruth, club.kit]),
            None,
            None
        )
    );
    // ...and he may open it to the space's readers again, unlogged: he
    // runs the space in his own right.
    let response = f
        .submit(
            &audience(thread),
            &[("read", "space"), ("reply", "space")],
            Some(&club.kit_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(club.visibility(thread).await, "public");
    let logged = f
        .id("SELECT count(*) FROM mod_actions WHERE action = 'restrict_thread'")
        .await;
    assert_eq!(logged, 0);
    // A moderator who doesn't run the space may too, and is logged.
    let response = f
        .submit(
            &audience(thread),
            &[
                ("read", "only"),
                ("read_group", &club.organisers.to_string()),
                ("reply", "space"),
            ],
            Some(&club.staff_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let logged = f
        .id("SELECT count(*) FROM mod_actions WHERE action = 'restrict_thread'")
        .await;
    assert_eq!(logged, 1);
    // Jonah neither wrote it nor runs the space, and can't read it now.
    let response = f.get_as(&audience(thread), Some(&club.jonah_cookie)).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    let _ = club.jonah;
}

#[sqlx::test]
async fn a_new_thread_can_be_restricted_from_the_start(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let page = f.page_as("/c/general/new", &club.ruth_cookie).await;
    assert!(page.contains("Only these people can read it"), "{page}");
    assert!(page.contains("Your Friends"), "{page}");
    // Visitors have the space's page cached.
    assert!(!f.page("/c/general").await.contains("Quiet word"));
    let friends = club.friends.to_string();
    let response = f
        .submit(
            "/c/general/new",
            &[
                ("title", "Quiet word"),
                ("body", "Hello @maya and @jonah."),
                ("read", "only"),
                ("read_group", &friends),
            ],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let thread = f
        .id("SELECT id FROM threads WHERE title = 'Quiet word'")
        .await;
    assert_eq!(
        club.stored(thread).await,
        (Some(vec![club.friends]), Some(vec![club.ruth]), None, None)
    );
    assert_eq!(club.visibility(thread).await, "restricted");
    // Only those it names see it, and only they are told of mentions.
    assert!(!f.page("/c/general").await.contains("Quiet word"));
    assert!(
        !f.page_as("/c/general", &club.maya_cookie)
            .await
            .contains("Quiet word")
    );
    assert!(
        f.page_as("/c/general", &club.jonah_cookie)
            .await
            .contains("Quiet word")
    );
    f.run_jobs().await;
    let mentioned = async |user: i64| {
        f.id(&format!(
            "SELECT count(*) FROM notifications WHERE user_id = {user} AND thread_id = {thread}"
        ))
        .await
    };
    assert_eq!(mentioned(club.maya).await, 0);
    assert_eq!(mentioned(club.jonah).await, 1);

    // Naming what isn't hers to name shows the form again with what she
    // wrote.
    let jonahs = f
        .id(&format!(
            "SELECT id FROM groups WHERE owner_user_id = {} AND name = 'Friends'",
            club.jonah
        ))
        .await;
    let response = f
        .submit(
            "/c/general/new",
            &[
                ("title", "Another word"),
                ("body", "Still quiet."),
                ("read", "only"),
                ("read_group", &jonahs.to_string()),
            ],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
    assert!(response.body.contains("Still quiet."), "{}", response.body);
    assert!(
        response.body.contains("Choose groups and lists"),
        "{}",
        response.body
    );
    // Leaving it to the space is the default.
    let response = f
        .submit(
            "/c/general/new",
            &[("title", "Loud word"), ("body", "For all.")],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(f.page("/c/general").await.contains("Loud word"));
    let _ = (club.kit, club.maya_cookie.len());
}

#[sqlx::test]
async fn a_held_new_thread_keeps_its_restriction(pool: PgPool) {
    let mut club = club(pool).await;
    club.f = club.f.without_rate_limits();
    let f = &club.f;
    // Tomasz is new, so his first posts are held.
    parlour::settings::set(
        &f.db,
        "messages",
        &parlour::settings::MessageSettings::default(),
    )
    .await
    .unwrap();
    let tomasz = f.member("tomasz", "user", None).await;
    let cookie = f.session(tomasz).await;
    let response = f
        .submit(
            "/c/general/new",
            &[
                ("title", "Hi from Leeds"),
                ("body", "Just to Maya."),
                ("read", "only"),
                ("read_users", "maya"),
            ],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(
        response.location().starts_with("/held/"),
        "{}",
        response.location()
    );
    let held = f.id("SELECT max(id) FROM held_posts").await;
    let response = f
        .submit(
            &format!("/mod/held/{held}/approve"),
            &[("reason", "")],
            Some(&club.staff_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let thread = f
        .id("SELECT id FROM threads WHERE title = 'Hi from Leeds'")
        .await;
    assert_eq!(
        club.stored(thread).await,
        (Some(vec![]), Some(vec![club.maya, tomasz]), None, None)
    );
    assert!(!f.page("/c/general").await.contains("Hi from Leeds"));
    assert!(
        f.page_as("/c/general", &club.maya_cookie)
            .await
            .contains("Hi from Leeds")
    );
    // The approval isn't for the public log.
    let public: bool = sqlx::query_scalar(
        "SELECT public FROM mod_actions WHERE action = 'approve_post' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&f.db)
    .await
    .unwrap();
    assert!(!public);
}

#[sqlx::test]
async fn a_held_new_thread_loses_a_list_deleted_while_it_waits(pool: PgPool) {
    let mut club = club(pool).await;
    club.f = club.f.without_rate_limits();
    let f = &club.f;
    // Tomasz is new, so his first posts are held.
    parlour::settings::set(
        &f.db,
        "messages",
        &parlour::settings::MessageSettings::default(),
    )
    .await
    .unwrap();
    let tomasz = f.member("tomasz", "user", None).await;
    let cookie = f.session(tomasz).await;
    let friends = f
        .id(&format!(
            "SELECT id FROM groups WHERE owner_user_id = {tomasz} AND name = 'Friends'"
        ))
        .await;
    f.join_group(friends, club.jonah).await;
    let response = f
        .submit(
            "/c/general/new",
            &[
                ("title", "Hi from Leeds"),
                ("body", "Just to my friends and Maya."),
                ("read", "only"),
                ("read_group", &friends.to_string()),
                ("read_users", "maya"),
            ],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(
        response.location().starts_with("/held/"),
        "{}",
        response.location()
    );
    // A held thread doesn't keep its list from being deleted.
    let response = f
        .submit(
            &format!("/settings/lists/{friends}/delete"),
            &[],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        f.id(&format!("SELECT count(*) FROM groups WHERE id = {friends}"))
            .await,
        0
    );
    let held = f.id("SELECT max(id) FROM held_posts").await;
    let response = f
        .submit(
            &format!("/mod/held/{held}/approve"),
            &[("reason", "")],
            Some(&club.staff_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    // The thread keeps the rest of its restriction, which is only narrower
    // for losing the list.
    let thread = f
        .id("SELECT id FROM threads WHERE title = 'Hi from Leeds'")
        .await;
    assert_eq!(
        club.stored(thread).await,
        (Some(vec![]), Some(vec![club.maya, tomasz]), None, None)
    );
    assert_eq!(club.visibility(thread).await, "restricted");
    assert!(!f.page("/c/general").await.contains("Hi from Leeds"));
    assert!(
        !f.page_as("/c/general", &club.jonah_cookie)
            .await
            .contains("Hi from Leeds")
    );
    assert!(
        f.page_as("/c/general", &club.maya_cookie)
            .await
            .contains("Hi from Leeds")
    );
}

#[sqlx::test]
async fn a_thread_called_audience_keeps_its_audience_page(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let created = f
        .thread(club.general, club.ruth, "Audience", "Who is out there?")
        .await;
    let slug: String = sqlx::query_scalar("SELECT slug FROM threads WHERE id = $1")
        .bind(created.thread_id)
        .fetch_one(&f.db)
        .await
        .unwrap();
    assert_eq!(slug, "audience-thread");
    let page = f
        .page_as(&audience(created.thread_id), &club.ruth_cookie)
        .await;
    assert!(page.contains("Who can take part in Audience"), "{page}");
}
