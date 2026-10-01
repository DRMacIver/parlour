//! Page handlers. Each reads through [`crate::content`] as the request's
//! [`Viewer`] and, for pages anonymous visitors may cache, attaches the
//! [`CacheTags`] the content layer collected.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::Uri;
use axum::response::{IntoResponse, Redirect, Response};

use crate::AppState;
use crate::content::{
    self, CategoryPage, Deps, Embed, IndexSection, LATEST_ON_INDEX, POSTS_PER_PAGE, PostBody,
    THREADS_PER_PAGE, ThreadPage, ThreadRow, page_count, page_of_seq,
};
use crate::error::AppError;
use crate::format;
use crate::pagination::{PageItem, Pagination, page_href, thread_list_pages};
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, cacheable};

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

/// Every page has exactly one URL. A request for any other spelling of it
/// (unknown query parameters, `?page=1`, `?page=02`, `/t/049/…`,
/// percent-encoded letters) is redirected there before any rendering, so
/// such URLs never cost a render or take up room in the anonymous cache,
/// which is keyed by the raw path and query.
fn redirect_unless_canonical(uri: &Uri, canonical: &str) -> Option<Response> {
    let requested = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
    (requested != canonical).then(|| Redirect::permanent(canonical).into_response())
}

/// Paths take ids as strings so that a malformed id is a 404, not a 400.
fn parse_id(id: &str) -> Option<i64> {
    id.parse().ok().filter(|&n: &i64| n >= 1)
}

/// Whether `slug` has the form the database requires of category and
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
    show_category: bool,
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
            show_category: true,
        },
        &deps,
    )
}

// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "category.html")]
struct CategoryTemplate {
    layout: Layout,
    category: CategoryPage,
    threads: Vec<ThreadRow>,
    pagination: Pagination,
    show_category: bool,
}

pub async fn category(
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
    let Some(mut category) =
        content::category_page(&state.db, &viewer, &slug, page, &mut deps).await?
    else {
        return web::not_found(&state, &viewer).await;
    };
    let total = page_count(category.thread_count, THREADS_PER_PAGE);
    if page > total {
        return web::not_found(&state, &viewer).await;
    }
    let threads = std::mem::take(&mut category.threads);
    let base = format!("/c/{}", category.category.slug);
    cacheable(
        &CategoryTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            category,
            threads,
            pagination: Pagination::new(base, page, total),
            show_category: false,
        },
        &deps,
    )
}

// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadTemplate {
    layout: Layout,
    thread: ThreadPage,
    pagination: Pagination,
}

impl ThreadTemplate {
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
        return web::not_found(&state, &viewer).await;
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
    cacheable(
        &ThreadTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            thread,
            pagination: Pagination::new(base, page, total),
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

/// Will jump to the first unread post once read markers exist (M4); for now
/// it goes to the start of the thread.
pub async fn thread_unread(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    redirect_to_thread(&state, &viewer, &id, Redirect::temporary).await
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
        None => web::not_found(state, viewer).await,
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
        Some(location) => {
            let href = page_href(
                &format::thread_href(&location.thread_id, &location.thread_slug),
                page_of_seq(location.seq),
            );
            Ok(Redirect::temporary(&format!("{href}#p{id}")).into_response())
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
