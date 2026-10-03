//! Page handlers. Each reads through [`crate::content`] as the request's
//! [`Viewer`] and, for pages anonymous visitors may cache, attaches the
//! [`CacheTags`] the content layer collected.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::Uri;
use axum::response::{IntoResponse, Redirect, Response};

use crate::AppState;
use crate::content::{
    self, Deps, Embed, IndexSection, LATEST_ON_INDEX, POSTS_PER_PAGE, PostBody, ReactionType,
    SpacePage, THREADS_PER_PAGE, ThreadPage, ThreadRow, page_count, page_of_seq,
};
use crate::error::AppError;
use crate::format;
use crate::notify;
use crate::pagination::{PageItem, Pagination, page_href, thread_list_pages};
use crate::social::{self, WatchLevel, Watched};
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, cacheable, redirect_unless_canonical};
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

/// Whether `slug` has the form the database requires of space and
/// thread slugs, so that it can appear in a URL unescaped.
fn well_formed_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
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
    sections: Vec<IndexSection>,
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
    let sections = content::index_sections(&state.db, &viewer, &mut deps).await?;
    let latest = content::latest(&state.db, &viewer, 1, LATEST_ON_INDEX, &mut deps).await?;
    cacheable(
        &IndexTemplate {
            layout: Layout::load(&state, &viewer, Nav::Index).await?,
            sections,
            latest: latest.threads,
        },
        &deps,
    )
}

// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "latest.html")]
struct LatestTemplate {
    layout: Layout,
    threads: Vec<ThreadRow>,
    pagination: Pagination,
    show_space: bool,
}

pub async fn latest(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    let Some(page) = requested_page(&uri) else {
        return web::not_found(&state, &viewer).await;
    };
    if let Some(redirect) = redirect_unless_canonical(&uri, &page_href("/latest", page)) {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let latest = content::latest(&state.db, &viewer, page, THREADS_PER_PAGE, &mut deps).await?;
    let total = page_count(latest.thread_count, THREADS_PER_PAGE);
    if page > total {
        return web::not_found(&state, &viewer).await;
    }
    cacheable(
        &LatestTemplate {
            layout: Layout::load(&state, &viewer, Nav::Latest).await?,
            threads: latest.threads,
            pagination: Pagination::new("/latest", page, total),
            show_space: true,
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
    /// How closely a logged-in viewer follows the space.
    watch: Option<WatchLevel>,
}

impl SpaceTemplate {
    fn watch_action(&self) -> String {
        format!("/c/{}/watch", self.space.space.slug)
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
    let can_post = match viewer.user() {
        None => false,
        Some(_) => content::space_target(&state.db, &viewer, &slug)
            .await?
            .is_some_and(|target| writing::may_post_in(&viewer, &target)),
    };
    let watch = match viewer.user() {
        Some(user) => {
            Some(social::watch_level(&state.db, user.id, Watched::Space(space.space.id)).await?)
        }
        None => None,
    };
    cacheable(
        &SpaceTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            space,
            threads,
            pagination: Pagination::new(base, page, total),
            show_space: false,
            logged_in: viewer.user().is_some(),
            can_post,
            watch,
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
    /// locked, or they are staff).
    can_edit: bool,
    /// The reactions a member can add.
    reaction_types: Vec<ReactionType>,
    /// Whether the viewer may react here: they are logged in and not
    /// silenced or suspended, and the thread isn't locked unless they are
    /// staff.
    can_react: bool,
    /// The post the "new since your last visit" divider goes before.
    divider_before: Option<i64>,
    /// How closely a logged-in viewer follows the thread.
    watch: Option<WatchLevel>,
    /// Whether the viewer is staff, who see moderation links.
    staff: bool,
}

impl ThreadTemplate {
    fn watch_action(&self) -> String {
        format!("/t/{}/watch", self.thread.thread.id)
    }

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
    let Some(canonical_slug) = content::thread_slug(&state.db, &viewer, id).await? else {
        return conversation_or_not_found(&state, &viewer, id).await;
    };
    let canonical = page_href(&format::thread_href(&id, &canonical_slug), page);
    if let Some(redirect) = redirect_unless_canonical(&uri, &canonical) {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let Some(thread) = content::thread_page(&state.db, &viewer, id, page, &mut deps).await? else {
        return web::not_found(&state, &viewer).await;
    };
    if thread.thread.slug != slug {
        let canonical = page_href(&format::thread_href(&id, &thread.thread.slug), page);
        return Ok(Redirect::permanent(&canonical).into_response());
    }
    let total = page_count(thread.last_seq, POSTS_PER_PAGE);
    if page > total {
        return web::not_found(&state, &viewer).await;
    }
    let base = format::thread_href(&id, &thread.thread.slug);
    let (reply_box, can_edit, reaction_types, divider_before, watch) = match viewer.user() {
        None => (ReplyBox::LogIn, false, Vec::new(), None, None),
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
                !thread.locked || viewer.is_staff(),
                content::reaction_types(&state.db, true).await?,
                divider_before,
                Some(WatchLevel::from_stored(thread.watch_level.as_deref())),
            )
        }
    };
    let can_react = viewer.user().is_some_and(|user| {
        writing::restriction(user).is_none() && (!thread.locked || viewer.is_staff())
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
            watch,
            staff: viewer.is_staff(),
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
    match content::thread_slug(&state.db, viewer, id).await? {
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
    match content::post_location(&state.db, &viewer, id).await? {
        Some(location) => Ok(Redirect::temporary(&location.href(id)).into_response()),
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
