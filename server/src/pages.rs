//! Page handlers. Each reads through [`crate::content`] as the request's
//! [`Viewer`] and, for pages anonymous visitors may cache, attaches the
//! [`CacheTags`] the content layer collected.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::Uri;
use axum::response::{IntoResponse, Redirect, Response};

use crate::AppState;
use crate::content::{
    self, Deps, Embed, FrontPage, LATEST_ON_INDEX, LatestScope, POSTS_PER_PAGE, PostBody,
    ReactionType, SectionTab, SpacePage, THREADS_PER_PAGE, ThreadPage, ThreadRow, page_count,
    page_of_seq,
};
use crate::error::AppError;
use crate::format;
use crate::layouts;
use crate::notify;
use crate::pagination::{PageItem, Pagination, page_href, thread_list_pages};
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, cacheable, redirect_unless_canonical, see_other};
use crate::writing;

/// The `?page=` number the request asks for: 1 if there is none, or `None`
/// (a 404) if it isn't a positive integer or is given more than once.
fn requested_page(uri: &Uri) -> Option<i64> {
    let Query(params) = Query::<Vec<(String, String)>>::try_from_uri(uri).ok()?;
    let mut pages = params.iter().filter(|(key, _)| key == "page");
    match (pages.next(), pages.next()) {
        (None, _) => Some(1),
        (Some((_, page)), None) => page.parse().ok().filter(|&n: &i64| n >= 1),
        (Some(_), Some(_)) => None,
    }
}

/// Paths take ids as strings so that a malformed id is a 404, not a 400.
pub(crate) fn parse_id(id: &str) -> Option<i64> {
    id.parse().ok().filter(|&n: &i64| n >= 1)
}

/// Whether `slug` has the form the database requires of space slugs, so
/// that it can appear in a URL unescaped: lower-case words with single
/// hyphens between them, or a feed's, which is its owner's username in
/// lower case and may hold underscores (DESIGN.md §3.2).
pub(crate) fn well_formed_slug(slug: &str) -> bool {
    let topic = !slug.is_empty()
        && slug.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    let feed = !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    topic || feed
}

impl ThreadRow {
    /// Posts the viewer hasn't read; 0 for visitors.
    fn new_posts(&self) -> i64 {
        self.unread.unwrap_or(0)
    }

    fn reply_count(&self) -> i64 {
        self.post_count - 1
    }

    /// Links to each page of a long thread, shown in thread lists.
    fn page_links(&self) -> Vec<PageItem> {
        thread_list_pages(page_count(self.post_count, POSTS_PER_PAGE))
    }
}

// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    layout: Layout,
    front: FrontPage,
    latest: Vec<ThreadRow>,
}

pub async fn index(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if let Some(redirect) = redirect_unless_canonical(&uri, "/") {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let front = content::front_page(&state.db, &viewer, &mut deps).await?;
    let latest = content::latest(
        &state.db,
        &viewer,
        LatestScope::All,
        1,
        LATEST_ON_INDEX,
        &mut deps,
    )
    .await?;
    cacheable(
        &IndexTemplate {
            layout: Layout::load(&state, &viewer, Nav::Index).await?,
            front,
            latest: latest.threads,
        },
        &deps,
    )
}

// ---------------------------------------------------------------------------

/// One tab of `/latest` (DESIGN.md §3.5).
struct Tab {
    label: String,
    href: String,
    current: bool,
}

#[derive(Template)]
#[template(path = "latest.html")]
struct LatestTemplate {
    layout: Layout,
    threads: Vec<ThreadRow>,
    pagination: Pagination,
    show_space: bool,
    scope: LatestScope,
    /// The section shown, for a section's tab.
    section: Option<SectionTab>,
    /// All, Front page (for members) and the sections with something in
    /// them to see, plus the one shown.
    tabs: Vec<Tab>,
    /// Whether the member has muted any space, which every tab leaves out.
    has_mutes: bool,
    logged_in: bool,
}

impl LatestTemplate {
    /// What the page is headed.
    fn heading(&self) -> String {
        match (&self.scope, &self.section) {
            (LatestScope::All, _) => "Latest".to_owned(),
            (LatestScope::Front, _) => "Front page".to_owned(),
            (LatestScope::Section(_), Some(section)) => section.name.clone(),
            (LatestScope::Section(_), None) => "Latest".to_owned(),
        }
    }

    /// What the list holds, said under the heading.
    fn note(&self) -> &'static str {
        match self.scope {
            LatestScope::All => "Every thread, most recently active first",
            LatestScope::Front => {
                "Threads in the spaces on your front page, most recently active first"
            }
            LatestScope::Section(_) => {
                "Threads in this section’s spaces, most recently active first"
            }
        }
    }

    fn is_all(&self) -> bool {
        self.scope == LatestScope::All
    }

    fn mark_read_action(&self) -> String {
        match &self.section {
            Some(section) => format!("/latest/s/{}/read", section.id),
            None => String::new(),
        }
    }
}

/// The address of a section's tab.
pub(crate) fn section_href(section_id: i64) -> String {
    format!("/latest/s/{section_id}")
}

/// All threads the viewer can read, by last activity.
pub async fn latest(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    render_latest(&state, &viewer, &uri, LatestScope::All, "/latest").await
}

/// The threads in the spaces on the viewer's front page. Visitors have no
/// front page of their own, so for them this is All.
pub async fn latest_front(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(see_other("/latest"));
    }
    render_latest(&state, &viewer, &uri, LatestScope::Front, "/latest/front").await
}

/// The threads in the spaces of one section of the viewer's layout.
pub async fn latest_section(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Response, AppError> {
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    render_latest(
        &state,
        &viewer,
        &uri,
        LatestScope::Section(id),
        &section_href(id),
    )
    .await
}

async fn render_latest(
    state: &AppState,
    viewer: &Viewer,
    uri: &Uri,
    scope: LatestScope,
    base: &str,
) -> Result<Response, AppError> {
    let Some(page) = requested_page(uri) else {
        return web::not_found(state, viewer).await;
    };
    if let Some(redirect) = redirect_unless_canonical(uri, &page_href(base, page)) {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let layout = content::layout_sections(&state.db, viewer, &mut deps).await?;
    let section = match scope {
        LatestScope::Section(id) => match layout.sections.iter().find(|s| s.id == id) {
            Some(section) => Some(section.clone()),
            None => return web::not_found(state, viewer).await,
        },
        _ => None,
    };
    let latest =
        content::latest(&state.db, viewer, scope, page, THREADS_PER_PAGE, &mut deps).await?;
    let total = page_count(latest.thread_count, THREADS_PER_PAGE);
    if page > total {
        return web::not_found(state, viewer).await;
    }
    let mut tabs = vec![Tab {
        label: "All".to_owned(),
        href: "/latest".to_owned(),
        current: scope == LatestScope::All,
    }];
    if viewer.user().is_some() {
        tabs.push(Tab {
            label: "Front page".to_owned(),
            href: "/latest/front".to_owned(),
            current: scope == LatestScope::Front,
        });
    }
    for s in &layout.sections {
        let current = scope == LatestScope::Section(s.id);
        if s.spaces > 0 || current {
            tabs.push(Tab {
                label: s.name.clone(),
                href: section_href(s.id),
                current,
            });
        }
    }
    cacheable(
        &LatestTemplate {
            layout: Layout::load(state, viewer, Nav::Latest).await?,
            threads: latest.threads,
            pagination: Pagination::new(base, page, total),
            show_space: true,
            scope,
            section,
            tabs,
            has_mutes: layout.has_mutes,
            logged_in: viewer.user().is_some(),
        },
        &deps,
    )
}

// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "space.html")]
struct SpaceTemplate {
    layout: Layout,
    space: SpacePage,
    threads: Vec<ThreadRow>,
    pagination: Pagination,
    show_space: bool,
    logged_in: bool,
    /// Whether the viewer may start a thread here.
    can_post: bool,
    /// Whether the viewer may change the space's settings.
    can_administer: bool,
    /// The section of a member's layout the space is in, if it is in one.
    following: Option<String>,
    /// Whether the member has muted the space.
    muted: bool,
    /// Essential spaces can't be muted (DESIGN.md §3.5).
    essential: bool,
    /// Whether this is the viewer's own feed (DESIGN.md §3.2).
    own_feed: bool,
}

impl SpaceTemplate {
    fn action(&self, what: &str) -> String {
        format!("/c/{}/{what}", self.space.space.slug)
    }
}

pub async fn space(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
    uri: Uri,
) -> Result<Response, AppError> {
    let Some(page) = requested_page(&uri).filter(|_| well_formed_slug(&slug)) else {
        return web::not_found(&state, &viewer).await;
    };
    if let Some(redirect) = redirect_unless_canonical(&uri, &page_href(&format!("/c/{slug}"), page))
    {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let Some(mut space) = content::space_page(&state.db, &viewer, &slug, page, &mut deps).await?
    else {
        return web::not_found(&state, &viewer).await;
    };
    let total = page_count(space.thread_count, THREADS_PER_PAGE);
    if page > total {
        return web::not_found(&state, &viewer).await;
    }
    let threads = std::mem::take(&mut space.threads);
    let base = format!("/c/{}", space.space.slug);
    let target = match viewer.user() {
        None => None,
        Some(_) => content::space_target(&state.db, &viewer, &slug).await?,
    };
    let can_post = target
        .as_ref()
        .is_some_and(|target| writing::may_start_in(&viewer, target));
    let can_administer = target
        .as_ref()
        .is_some_and(|target| target.rules.may_administer(&viewer));
    let (following, muted) = match viewer.user() {
        Some(user) => {
            let layout = layouts::id_for(&state.db, Some(user.id)).await?;
            (
                layouts::placement(&state.db, layout, space.space.id)
                    .await?
                    .map(|p| p.section_name),
                layouts::is_muted(&state.db, user.id, space.space.id).await?,
            )
        }
        None => (None, false),
    };
    let essential = target.as_ref().is_some_and(|target| target.essential);
    let own_feed = target
        .as_ref()
        .is_some_and(|target| target.owned_by(&viewer));
    // Under a break-glass grant the page is for reading only.
    let logged_in = viewer.user().is_some() && !space.granted;
    cacheable(
        &SpaceTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            space,
            threads,
            pagination: Pagination::new(base, page, total),
            show_space: false,
            logged_in,
            can_post,
            can_administer,
            following,
            muted,
            essential,
            own_feed,
        },
        &deps,
    )
}

// ---------------------------------------------------------------------------

/// What goes at the foot of a thread page.
enum ReplyBox {
    /// The quick reply form.
    Form,
    /// Why this member can't reply.
    Note(&'static str),
    /// An invitation to log in, for visitors.
    LogIn,
}

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadTemplate {
    layout: Layout,
    thread: ThreadPage,
    pagination: Pagination,
    /// The logged-in viewer, if any.
    viewer_id: Option<i64>,
    reply_box: ReplyBox,
    /// Whether the viewer may edit their own posts here (the thread isn't
    /// locked, or they moderate here).
    can_edit: bool,
    /// The reactions a member can add.
    reaction_types: Vec<ReactionType>,
    /// Whether the viewer may react here: they are logged in and not
    /// silenced or suspended, and the thread isn't locked unless they
    /// moderate here.
    can_react: bool,
    /// The post the "new since your last visit" divider goes before.
    divider_before: Option<i64>,
    /// Whether a logged-in viewer is subscribed to the thread (`None` for
    /// visitors, who have no toggle).
    subscribed: Option<bool>,
    /// Whether the viewer moderates here, staff or one of those who run the
    /// space, who see moderation links (DESIGN.md §3.1).
    moderates: bool,
    /// Whether the viewer may change who can read and reply in the
    /// thread: its author, and those who run its space (DESIGN.md §3.4).
    may_restrict: bool,
}

impl ThreadTemplate {
    fn logged_in(&self) -> bool {
        self.viewer_id.is_some()
    }

    fn divider_is(&self, post_id: &i64) -> bool {
        self.divider_before == Some(*post_id)
    }

    /// Whether a post that moved away from here has since moved back, so
    /// that the post itself, not its old place, has its anchor.
    fn moved_back(&self, to: &Option<content::ThreadRef>) -> bool {
        to.as_ref().is_some_and(|t| t.id == self.thread.thread.id)
    }

    fn login_href(&self) -> String {
        format!(
            "/login?next={}",
            web::query_encode(&self.pagination.href(&self.pagination.current))
        )
    }

    fn can_reply(&self) -> bool {
        matches!(self.reply_box, ReplyBox::Form)
    }

    fn is_mine(&self, author_id: &i64) -> bool {
        self.viewer_id == Some(*author_id)
    }

    /// Where an "in reply to" link should point: an anchor if the post is
    /// on this page, otherwise its permalink.
    fn reply_href(&self, post_id: &i64, seq: &i64) -> String {
        if page_of_seq(*seq) == self.pagination.current {
            format!("#p{post_id}")
        } else {
            format!("/p/{post_id}")
        }
    }
}

pub async fn thread(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((id, slug)): Path<(String, String)>,
    uri: Uri,
) -> Result<Response, AppError> {
    let (Some(id), Some(page)) = (parse_id(&id), requested_page(&uri)) else {
        return web::not_found(&state, &viewer).await;
    };
    // The slug is checked first with a cheap query, so that a wrong one
    // costs no more than any other non-canonical URL.
    let Some(canonical_slug) = content::thread_slug(&state.db, &viewer, id, Some(&slug)).await?
    else {
        return conversation_or_not_found(&state, &viewer, id).await;
    };
    let canonical = page_href(&format::thread_href(&id, &canonical_slug), page);
    if let Some(redirect) = redirect_unless_canonical(&uri, &canonical) {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let Some(mut thread) = content::thread_page(&state.db, &viewer, id, page, &mut deps).await?
    else {
        return web::not_found(&state, &viewer).await;
    };
    // Visitors never read a thread with a read restriction, and never
    // reply, so a reply restriction is nothing to them; leaving it out
    // also keeps the names it gives out of the cached page.
    if viewer.user().is_none() {
        thread.reply_label = None;
    }
    if thread.thread.slug != slug {
        let canonical = page_href(&format::thread_href(&id, &thread.thread.slug), page);
        return Ok(Redirect::permanent(&canonical).into_response());
    }
    let total = page_count(thread.last_seq, POSTS_PER_PAGE);
    if page > total {
        return web::not_found(&state, &viewer).await;
    }
    let base = format::thread_href(&id, &thread.thread.slug);
    let moderates = thread.space.rules.may_administer(&viewer);
    let (reply_box, can_edit, reaction_types, divider_before, subscribed) = match viewer.user() {
        None => (ReplyBox::LogIn, false, Vec::new(), None, None),
        // Under a break-glass grant the page is for reading: nothing to
        // reply, react or subscribe with (DESIGN.md §8).
        Some(_) if thread.granted => (
            ReplyBox::Note("You can’t reply in this thread."),
            false,
            Vec::new(),
            None,
            None,
        ),
        Some(user) => {
            let reply_box = match writing::reply_refusal(&state, &viewer, &thread.target()).await? {
                None => ReplyBox::Form,
                Some(reason) => ReplyBox::Note(reason),
            };
            let divider_before = thread.read_marker.and_then(|marker| {
                thread
                    .posts
                    .iter()
                    .find(|post| post.seq > marker)
                    .map(|post| post.id)
            });
            if let Some(last) = thread.posts.last() {
                notify::seen_thread(&state, user.id, id, last.seq).await?;
            }
            (
                reply_box,
                !thread.locked || moderates,
                content::reaction_types(&state.db, true).await?,
                divider_before,
                Some(thread.subscribed),
            )
        }
    };
    let may_restrict = viewer.user().is_some_and(|user| {
        user.id == thread.author.id || thread.space.rules.may_administer(&viewer)
    });
    let can_react = !thread.granted
        && viewer.user().is_some_and(|user| {
            writing::restriction(user).is_none() && (!thread.locked || moderates)
        });
    cacheable(
        &ThreadTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            thread,
            pagination: Pagination::new(base, page, total),
            viewer_id: viewer.user_id(),
            reply_box,
            can_edit,
            reaction_types,
            can_react,
            divider_before,
            subscribed,
            moderates,
            may_restrict,
        },
        &deps,
    )
}

pub async fn thread_without_slug(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    redirect_to_thread(&state, &viewer, &id, Redirect::permanent).await
}

/// Jumps to the first post a member hasn't read (DESIGN.md §3.5). Visitors
/// go to the start of the thread.
pub async fn thread_unread(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return redirect_to_thread(&state, &viewer, &id, Redirect::temporary).await;
    }
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match content::first_unread(&state.db, &viewer, id).await? {
        Some((post_id, location)) => {
            Ok(Redirect::temporary(&location.href(post_id)).into_response())
        }
        // A thread merged into another: what is unread there.
        None => match content::moderation::merged_into(&state.db, &viewer, id).await? {
            Some((to, _)) => Ok(Redirect::temporary(&format!("/t/{to}/unread")).into_response()),
            None => web::not_found(&state, &viewer).await,
        },
    }
}

async fn redirect_to_thread(
    state: &AppState,
    viewer: &Viewer,
    id: &str,
    redirect: impl FnOnce(&str) -> Redirect,
) -> Result<Response, AppError> {
    let Some(id) = parse_id(id) else {
        return web::not_found(state, viewer).await;
    };
    match content::thread_slug(&state.db, viewer, id, None).await? {
        Some(slug) => Ok(redirect(&format::thread_href(&id, &slug)).into_response()),
        None => conversation_or_not_found(state, viewer, id).await,
    }
}

/// For a thread id that is one of the viewer's conversations, a redirect
/// there; for anything else, not found.
async fn conversation_or_not_found(
    state: &AppState,
    viewer: &Viewer,
    id: i64,
) -> Result<Response, AppError> {
    match content::thread_target(&state.db, viewer, id).await? {
        Some(target) if target.conversation().is_some() => {
            Ok(Redirect::temporary(&target.href()).into_response())
        }
        // A thread merged into another leads there.
        _ => match content::moderation::merged_into(&state.db, viewer, id).await? {
            Some((to, slug)) => {
                Ok(Redirect::temporary(&format::thread_href(&to, &slug)).into_response())
            }
            None => web::not_found(state, viewer).await,
        },
    }
}

pub async fn permalink(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match content::post_location_granted(&state.db, &viewer, id).await? {
        Some(location) => Ok(Redirect::temporary(&location.href(id)).into_response()),
        // An author's post in a thread they can no longer read: to what
        // they can still see of it (DESIGN.md §3.3).
        None if content::post_target_or_own(&state.db, &viewer, id)
            .await?
            .is_some() =>
        {
            Ok(Redirect::temporary(&format!("/p/{id}/revisions")).into_response())
        }
        None => web::not_found(&state, &viewer).await,
    }
}

// ---------------------------------------------------------------------------

pub async fn not_found(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    web::not_found(&state, &viewer).await
}
