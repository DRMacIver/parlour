//! The pages for direct messages (DESIGN.md §3.7; Messages.dc.html): the
//! list of conversations at `/messages`, one conversation at
//! `/messages/{id}`, starting one at `/messages/new`, leaving and adding
//! people, and blocking members at `/u/{username}/block`.
//!
//! Everything here is for logged-in members only, so none of it is ever
//! cached. Replying, editing, deleting and reacting use the usual post
//! pages; the decisions are made in [`crate::messages`].

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};

use crate::AppState;
use crate::content::messages::{
    CONVERSATIONS_PER_PAGE, ConversationList, ConversationPage, ConversationRow,
};
use crate::content::{self, Deps, POSTS_PER_PAGE, PostBody, ReactionType, page_count, page_of_seq};
use crate::error::AppError;
use crate::messages;
use crate::notify;
use crate::pages::parse_id;
use crate::pagination::{PageItem, Pagination};
use crate::settings;
use crate::social::{self, DmPolicy, WatchLevel, Watched};
use crate::store;
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, render, see_other};
use crate::write_pages::not_allowed;
use crate::writing::Refused;

type Fields = Vec<(String, String)>;

fn field<'a>(fields: &'a Fields, name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// The `?page=` number asked for, or `None` for anything malformed.
fn requested_page(uri: &Uri) -> Option<i64> {
    let Query(params) = Query::<Fields>::try_from_uri(uri).ok()?;
    match params.as_slice() {
        [] => Some(1),
        [(key, value)] if key == "page" => value
            .parse()
            .ok()
            .filter(|&n: &i64| n >= 1 && value == &n.to_string()),
        _ => None,
    }
}

/// What the list beside a conversation says about messaging in general.
struct ListContext {
    list: ConversationList,
    pagination: Pagination,
    policy: DmPolicy,
    /// Why the viewer can't start conversations, if they can't.
    start_refusal: Option<String>,
    turned_off: bool,
}

async fn list_context(
    state: &AppState,
    viewer: &Viewer,
    page: i64,
) -> Result<ListContext, AppError> {
    let user = viewer.user().expect("only members see their messages");
    let list = content::messages::conversations(state.db(), viewer, page).await?;
    let total = page_count(list.total, CONVERSATIONS_PER_PAGE);
    Ok(ListContext {
        pagination: Pagination::new("/messages", page, total),
        list,
        policy: social::dm_policy(state.db(), user.id).await?,
        start_refusal: messages::start_refusal(state, user).await?,
        turned_off: !settings::message_settings(state.db()).await?.enabled,
    })
}

impl ConversationRow {
    fn name(&self, viewer_id: &Option<i64>) -> String {
        self.conversation.name(&self.title, *viewer_id)
    }

    fn people(&self, viewer_id: &Option<i64>) -> String {
        self.conversation.people(*viewer_id)
    }

    /// Whose avatar stands for the conversation: the first other person.
    fn face(&self, viewer_id: &Option<i64>) -> content::UserRef {
        self.conversation
            .others(*viewer_id)
            .first()
            .map(|p| p.user.clone())
            .unwrap_or_else(|| self.conversation.started_by.clone())
    }

    fn href(&self) -> String {
        if self.unread > 0 {
            format!("/messages/{}/unread", self.id)
        } else {
            format!("/messages/{}", self.id)
        }
    }
}

// ---------------------------------------------------------------------------
// The list

#[derive(Template)]
#[template(path = "messages.html")]
struct MessagesTemplate {
    layout: Layout,
    ctx: ListContext,
    viewer_id: Option<i64>,
    current: Option<i64>,
    /// The conversation being shown, if any.
    shown: Option<Shown>,
}

/// The conversation shown beside the list.
struct Shown {
    page: ConversationPage,
    pagination: Pagination,
    divider_before: Option<i64>,
    reaction_types: Vec<ReactionType>,
    /// Why the viewer can't send here, if they can't.
    refusal: Option<&'static str>,
    muted: bool,
    /// The one other person, in a conversation between two, if they can
    /// be blocked.
    blockable: Option<content::UserRef>,
    errors: Vec<String>,
    draft: String,
}

impl MessagesTemplate {
    fn errors(&self) -> Vec<String> {
        self.shown
            .as_ref()
            .map(|s| s.errors.clone())
            .unwrap_or_default()
    }

    fn is_mine(&self, author_id: &i64) -> bool {
        self.viewer_id == Some(*author_id)
    }

    fn is_current(&self, id: &i64) -> bool {
        self.current == Some(*id)
    }

    fn divider_is(&self, post_id: &i64) -> bool {
        self.shown
            .as_ref()
            .is_some_and(|s| s.divider_before == Some(*post_id))
    }

    fn reply_href(&self, post_id: &i64, seq: &i64) -> String {
        match &self.shown {
            Some(shown) if page_of_seq(*seq) == shown.pagination.current => format!("#p{post_id}"),
            _ => format!("/p/{post_id}"),
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/messages"));
    }
    let Some(page) = requested_page(&uri) else {
        return web::not_found(&state, &viewer).await;
    };
    let ctx = list_context(&state, &viewer, page).await?;
    if page > ctx.pagination.total {
        return web::not_found(&state, &viewer).await;
    }
    render(
        StatusCode::OK,
        &MessagesTemplate {
            layout: Layout::load(&state, &viewer, Nav::Messages).await?,
            ctx,
            viewer_id: viewer.user_id(),
            current: None,
            shown: None,
        },
    )
}

// ---------------------------------------------------------------------------
// One conversation

async fn render_conversation(
    state: &AppState,
    viewer: &Viewer,
    id: i64,
    page: i64,
    status: StatusCode,
    errors: Vec<String>,
    draft: String,
) -> Result<Response, AppError> {
    let user = viewer.user().expect("only members see conversations");
    let mut deps = Deps::new();
    let Some(conversation) =
        content::messages::conversation_page(state.db(), viewer, id, page, &mut deps).await?
    else {
        return web::not_found(state, viewer).await;
    };
    let total = page_count(conversation.last_seq, POSTS_PER_PAGE);
    if page > total {
        return web::not_found(state, viewer).await;
    }
    let divider_before = conversation.read_marker.and_then(|marker| {
        conversation
            .posts
            .iter()
            .find(|post| post.seq > marker)
            .map(|post| post.id)
    });
    if let Some(last) = conversation.posts.last() {
        store::mark_read(state.db(), user.id, id, last.seq).await?;
        notify::seen_thread(state, user.id, id, last.seq).await?;
    }
    let target = content::thread_target(state.db(), viewer, id)
        .await?
        .expect("a conversation the viewer can read");
    let refusal = messages::send_refusal(state, viewer, &target).await?;
    let others = conversation.conversation.others(Some(user.id));
    let blockable = match others.as_slice() {
        [other] if !other.staff && !conversation.conversation.staff_message => {
            Some(other.user.clone())
        }
        _ => None,
    };
    let muted =
        social::watch_level(state.db(), user.id, Watched::Thread(id)).await? == WatchLevel::Muted;
    let shown = Shown {
        pagination: Pagination::new(format!("/messages/{id}"), page, total),
        page: conversation,
        divider_before,
        reaction_types: content::reaction_types(state.db(), true).await?,
        refusal,
        muted,
        blockable,
        errors,
        draft,
    };
    render(
        status,
        &MessagesTemplate {
            layout: Layout::load(state, viewer, Nav::Messages).await?,
            ctx: list_context(state, viewer, 1).await?,
            viewer_id: viewer.user_id(),
            current: Some(id),
            shown: Some(shown),
        },
    )
}

pub async fn conversation(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Response, AppError> {
    let back = uri
        .path_and_query()
        .map_or(uri.path(), |pq| pq.as_str())
        .to_owned();
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let (Some(id), Some(page)) = (parse_id(&id), requested_page(&uri)) else {
        return web::not_found(&state, &viewer).await;
    };
    render_conversation(
        &state,
        &viewer,
        id,
        page,
        StatusCode::OK,
        Vec::new(),
        String::new(),
    )
    .await
}

/// Jumps to the first message the viewer hasn't read.
pub async fn unread(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first(&format!("/messages/{id}")));
    }
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match content::first_unread(state.db(), &viewer, id).await? {
        Some((post_id, location)) if location.private => {
            Ok(Redirect::temporary(&location.href(post_id)).into_response())
        }
        _ => web::not_found(&state, &viewer).await,
    }
}

// ---------------------------------------------------------------------------
// Starting a conversation

#[derive(Template)]
#[template(path = "message_new.html")]
struct NewMessageTemplate {
    layout: Layout,
    to: String,
    title: String,
    body: String,
    preview: Option<String>,
    errors: Vec<String>,
}

pub async fn new_page(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    let back = uri
        .path_and_query()
        .map_or(uri.path(), |pq| pq.as_str())
        .to_owned();
    let Some(user) = viewer.user() else {
        return Ok(web::login_first(&back));
    };
    if let Some(why) = messages::start_refusal(&state, user).await? {
        return not_allowed(&state, &viewer, &why).await;
    }
    let params = Query::<Fields>::try_from_uri(&uri)
        .map(|Query(params)| params)
        .unwrap_or_default();
    render(
        StatusCode::OK,
        &NewMessageTemplate {
            layout: Layout::load(&state, &viewer, Nav::Messages).await?,
            to: field(&params, "to").unwrap_or_default().to_owned(),
            title: String::new(),
            body: String::new(),
            preview: None,
            errors: Vec::new(),
        },
    )
}

pub async fn start(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/messages/new"));
    }
    let to = field(&fields, "to").unwrap_or_default();
    let title = field(&fields, "title").unwrap_or_default();
    let body = field(&fields, "body").unwrap_or_default();
    let form = |status: StatusCode, errors: Vec<String>, preview: Option<String>| {
        let state = state.clone();
        let viewer = viewer.clone();
        async move {
            render(
                status,
                &NewMessageTemplate {
                    layout: Layout::load(&state, &viewer, Nav::Messages).await?,
                    to: to.to_owned(),
                    title: title.to_owned(),
                    body: body.to_owned(),
                    preview,
                    errors,
                },
            )
        }
    };
    if field(&fields, "preview").is_some() {
        let clean = crate::writing::clean_body(body);
        let mut problems = Vec::new();
        crate::writing::check_body(&clean, &mut problems);
        if !problems.is_empty() {
            return form(StatusCode::BAD_REQUEST, problems, None).await;
        }
        if !crate::writing::may_preview(&state, &viewer) {
            return form(
                StatusCode::TOO_MANY_REQUESTS,
                vec!["You’re previewing faster than this forum allows. Wait a few seconds, then try again.".to_owned()],
                None,
            )
            .await;
        }
        let html = store::render_post(state.db(), &clean).await?.html;
        return form(StatusCode::OK, Vec::new(), Some(html)).await;
    }
    match messages::start(&state, &viewer, to, title, body).await? {
        Ok(posted) => Ok(see_other(&format!(
            "/messages/{}#p{}",
            posted.thread_id, posted.post_id
        ))),
        Err(Refused::Invalid(problems)) => form(StatusCode::BAD_REQUEST, problems, None).await,
        Err(Refused::RateLimited) => {
            form(
                StatusCode::TOO_MANY_REQUESTS,
                vec!["You’re starting conversations faster than this forum allows. Wait a few minutes, then send it again.".to_owned()],
                None,
            )
            .await
        }
        Err(Refused::NotAllowedBecause(why)) => {
            form(StatusCode::FORBIDDEN, vec![why], None).await
        }
        Err(Refused::NotAllowed(why)) => {
            form(StatusCode::FORBIDDEN, vec![why.to_owned()], None).await
        }
        Err(Refused::LogIn) => Ok(web::login_first("/messages/new")),
        Err(Refused::NotFound) => web::not_found(&state, &viewer).await,
    }
}

// ---------------------------------------------------------------------------
// People

pub async fn add_person(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let Some(thread_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let username = field(&fields, "username").unwrap_or_default();
    match messages::add_person(&state, &viewer, thread_id, username).await? {
        Ok(_) => Ok(see_other(&format!("/messages/{thread_id}"))),
        Err(Refused::LogIn) => Ok(web::login_first(&format!("/messages/{thread_id}"))),
        Err(Refused::NotFound) => web::not_found(&state, &viewer).await,
        Err(Refused::NotAllowed(why)) => not_allowed(&state, &viewer, why).await,
        Err(Refused::NotAllowedBecause(why)) => not_allowed(&state, &viewer, &why).await,
        Err(Refused::Invalid(problems)) => {
            render_conversation(
                &state,
                &viewer,
                thread_id,
                1,
                StatusCode::BAD_REQUEST,
                problems,
                String::new(),
            )
            .await
        }
        Err(Refused::RateLimited) => web::rate_limited(&state, &viewer).await,
    }
}

pub async fn leave(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let Some(thread_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match messages::leave(&state, &viewer, thread_id).await? {
        Ok(()) => Ok(see_other("/messages")),
        Err(Refused::LogIn) => Ok(web::login_first(&format!("/messages/{thread_id}"))),
        Err(_) => web::not_found(&state, &viewer).await,
    }
}

// ---------------------------------------------------------------------------
// Blocking

#[derive(Template)]
#[template(path = "block.html")]
struct BlockTemplate {
    layout: Layout,
    member: content::UserRef,
}

pub async fn block_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
) -> Result<Response, AppError> {
    match messages::member_to_block(&state, &viewer, &username).await? {
        Ok(member) => render(
            StatusCode::OK,
            &BlockTemplate {
                layout: Layout::load(&state, &viewer, Nav::None).await?,
                member: member.user,
            },
        ),
        Err(refusal) => block_refused(&state, &viewer, &username, refusal).await,
    }
}

async fn block_refused(
    state: &AppState,
    viewer: &Viewer,
    username: &str,
    refusal: Refused,
) -> Result<Response, AppError> {
    match refusal {
        Refused::LogIn => Ok(web::login_first(&format!("/u/{username}/block"))),
        Refused::NotAllowed(why) => not_allowed(state, viewer, why).await,
        Refused::NotAllowedBecause(why) => not_allowed(state, viewer, &why).await,
        Refused::RateLimited => web::rate_limited(state, viewer).await,
        Refused::NotFound | Refused::Invalid(_) => web::not_found(state, viewer).await,
    }
}

pub async fn block(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
) -> Result<Response, AppError> {
    match messages::block(&state, &viewer, &username, true).await? {
        Ok(member) => Ok(see_other(&format!("/u/{}", member.user.username))),
        Err(refusal) => block_refused(&state, &viewer, &username, refusal).await,
    }
}

pub async fn unblock(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
) -> Result<Response, AppError> {
    match messages::block(&state, &viewer, &username, false).await? {
        Ok(_) => Ok(see_other("/settings?done=unblocked#blocked")),
        Err(refusal) => block_refused(&state, &viewer, &username, refusal).await,
    }
}
