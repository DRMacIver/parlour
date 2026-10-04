//! `/search` (DESIGN.md §9, §11): a form, and the posts it finds that the
//! viewer may read, best match or newest first.
//!
//! Results depend on the viewer and change with every post, so they are
//! never cached; searching is rate limited per client address and, for
//! members, per account.

use askama::Template;
use axum::extract::{Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;

use crate::AppState;
use crate::client::ClientIp;
use crate::content::search::{
    self, HitPlace, MAX_QUERY_CHARS, SearchHit, SearchOutcome, SearchQuery,
};
use crate::content::{self, SpaceTarget};
use crate::error::AppError;
use crate::ratelimit::{Action, Scope};
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, query_encode, render};

pub const RESULTS_PER_PAGE: i64 = 20;
/// Searches go no deeper than this many pages.
pub const MAX_PAGES: i64 = 25;

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    layout: Layout,
    query: SearchQuery,
    spaces: Vec<SpaceTarget>,
    /// Whether a search was made (there was a query).
    searched: bool,
    no_terms: bool,
    hits: Vec<SearchHit>,
    page: i64,
    prev: Option<String>,
    next: Option<String>,
    error: Option<String>,
}

impl SearchTemplate {
    fn space_selected(&self, slug: &str) -> bool {
        self.query.space.as_deref() == Some(slug)
    }
}

/// The URL of another page of the same search.
fn page_url(query: &SearchQuery, page: i64) -> String {
    let mut url = format!("/search?q={}", query_encode(&query.text));
    if let Some(space) = &query.space {
        url.push_str(&format!("&in={}", query_encode(space)));
    }
    if let Some(author) = &query.author {
        url.push_str(&format!("&by={}", query_encode(author)));
    }
    if query.newest_first {
        url.push_str("&sort=new");
    }
    if page > 1 {
        url.push_str(&format!("&page={page}"));
    }
    url
}

fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Whether the client may search now, using up some of their allowance.
fn allowed(state: &AppState, ip: &ClientIp, viewer: &Viewer) -> bool {
    let limiter = state.limiter();
    let by_address = limiter.check(Action::Search, Scope::Ip, &ip.0);
    let by_account = match viewer.user_id() {
        Some(id) => limiter.check(Action::Search, Scope::User, &id.to_string()),
        None => true,
    };
    by_address && by_account
}

pub async fn search_page(
    State(state): State<AppState>,
    viewer: Viewer,
    ip: ClientIp,
    uri: Uri,
) -> Result<Response, AppError> {
    let params = Query::<Vec<(String, String)>>::try_from_uri(&uri)
        .map(|Query(params)| params)
        .unwrap_or_default();
    let blank = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let query = SearchQuery {
        // Postgres text can't hold NUL, and no other control character
        // means anything in a search.
        text: param(&params, "q")
            .unwrap_or_default()
            .replace(char::is_control, " ")
            .trim()
            .to_owned(),
        space: blank(param(&params, "in")),
        author: blank(param(&params, "by")).map(|a| a.trim_start_matches('@').to_owned()),
        newest_first: param(&params, "sort") == Some("new"),
    };
    let page = match param(&params, "page") {
        None => 1,
        Some(page) => match page.parse::<i64>() {
            Ok(n) if (1..=MAX_PAGES).contains(&n) => n,
            _ => return web::not_found(&state, &viewer).await,
        },
    };
    let mut template = SearchTemplate {
        layout: Layout::load(&state, &viewer, Nav::Search).await?,
        // Feeds without threads are left out (DESIGN.md §3.2).
        spaces: content::readable_spaces(state.db(), &viewer)
            .await?
            .into_iter()
            .filter(|c| c.listed())
            .collect(),
        searched: !query.text.is_empty(),
        no_terms: false,
        hits: Vec::new(),
        page,
        prev: None,
        next: None,
        error: None,
        query,
    };
    if !template.searched {
        return render(StatusCode::OK, &template);
    }
    let too_long = if template.query.text.chars().count() > MAX_QUERY_CHARS {
        Some(format!(
            "Searches can be at most {MAX_QUERY_CHARS} characters long."
        ))
    } else if template.query.author.as_ref().is_some_and(|a| a.len() > 64) {
        Some("Usernames are at most 64 characters long.".to_owned())
    } else if template.query.space.as_ref().is_some_and(|c| c.len() > 100) {
        Some("Space names are at most 100 characters long.".to_owned())
    } else {
        None
    };
    if let Some(error) = too_long {
        template.error = Some(error);
        return render(StatusCode::BAD_REQUEST, &template);
    }
    if !allowed(&state, &ip, &viewer) {
        return web::rate_limited(&state, &viewer).await;
    }
    match search::search(
        state.db(),
        &viewer,
        &template.query,
        (page - 1) * RESULTS_PER_PAGE,
        RESULTS_PER_PAGE,
    )
    .await?
    {
        SearchOutcome::NoTerms => template.no_terms = true,
        SearchOutcome::Hits { hits, more } => {
            template.hits = hits;
            template.prev = (page > 1).then(|| page_url(&template.query, page - 1));
            template.next = (more && page < MAX_PAGES).then(|| page_url(&template.query, page + 1));
        }
    }
    render(StatusCode::OK, &template)
}

impl SearchHit {
    /// Where it was found, as a link.
    fn place_href(&self) -> String {
        match &self.place {
            HitPlace::Space { slug, .. } => format!("/c/{slug}"),
            HitPlace::Conversation { .. } => format!("/messages/{}", self.thread_id),
        }
    }

    fn place_name(&self) -> &str {
        match &self.place {
            HitPlace::Space { name, .. } => name,
            HitPlace::Conversation { name, .. } => name,
        }
    }

    /// What the result is headed with: its thread's title, or for a
    /// conversation, its name for this viewer (which a conversation
    /// without a title needs).
    fn title(&self) -> &str {
        match &self.place {
            HitPlace::Space { .. } => &self.thread_title,
            HitPlace::Conversation { name, .. } => name,
        }
    }

    /// For a conversation, who else is in it ("" if nobody); `None`
    /// otherwise.
    fn conversation_with(&self) -> Option<&str> {
        match &self.place {
            HitPlace::Space { .. } => None,
            HitPlace::Conversation { with, .. } => Some(with),
        }
    }
}
