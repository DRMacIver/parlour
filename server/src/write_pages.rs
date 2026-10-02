//! Pages for writing (DESIGN.md §3.2–3.5): the composer for new threads,
//! replies, edits and quote-forks, with a server-rendered preview; deleting
//! a post; reactions; edit history; marking a category read; and the
//! admin's list of reactions.
//!
//! Every form is a plain HTML form that works without JavaScript. The
//! decisions are made in [`crate::writing`].

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};

use crate::AppState;
use crate::changes::Change;
use crate::content::{
    self, CategoryTarget, Deps, PostHistory, PostReactions, PostTarget, ReactionType, ThreadTarget,
};
use crate::diff::{self, Segment};
use crate::error::AppError;
use crate::format;
use crate::pages::parse_id;
use crate::store;
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, cacheable, render, see_other};
use crate::writing::{self, MAX_REACTIONS, Posted, Refused};

type Fields = Vec<(String, String)>;

fn field<'a>(fields: &'a Fields, name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// The page and anchor where a post is.
async fn post_href(state: &AppState, viewer: &Viewer, post_id: i64) -> Result<String, AppError> {
    Ok(
        match content::post_location(state.db(), viewer, post_id).await? {
            Some(location) => location.href(post_id),
            None => "/".to_owned(),
        },
    )
}

pub(crate) async fn not_allowed(
    state: &AppState,
    viewer: &Viewer,
    reason: &str,
) -> Result<Response, AppError> {
    web::message(
        state,
        viewer,
        StatusCode::FORBIDDEN,
        "Not allowed",
        &[reason],
        Some(("/", "Back to the forum index")),
    )
    .await
}

const TOO_FAST: &str =
    "You’re posting faster than this forum allows. Wait a minute, then send it again.";

const TOO_FAST_PREVIEW: &str =
    "You’re previewing faster than this forum allows. Wait a few seconds, then try again.";

// ---------------------------------------------------------------------------
// The composer

/// What the composer is for.
enum Compose {
    NewThread(CategoryTarget),
    Reply(ThreadTarget),
    Edit(PostTarget),
    Fork(PostTarget, Vec<CategoryTarget>),
}

/// What has been typed so far.
#[derive(Default)]
struct Draft {
    title: String,
    body: String,
    reply_to: Option<i64>,
    category: String,
}

impl Draft {
    fn from_fields(fields: &Fields) -> Draft {
        Draft {
            title: field(fields, "title").unwrap_or_default().to_owned(),
            body: field(fields, "body").unwrap_or_default().to_owned(),
            reply_to: field(fields, "reply_to").and_then(|v| v.trim().parse().ok()),
            category: field(fields, "category").unwrap_or_default().to_owned(),
        }
    }
}

struct Crumb {
    href: String,
    label: String,
}

struct ReplyChoice {
    post_id: i64,
    seq: i64,
    username: String,
}

struct CategoryOption {
    slug: String,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "compose.html")]
struct ComposeTemplate {
    layout: Layout,
    crumbs: Vec<Crumb>,
    heading: String,
    form_label: &'static str,
    action: String,
    title: Option<String>,
    categories: Vec<CategoryOption>,
    reply_to: Option<ReplyChoice>,
    body: String,
    body_label: &'static str,
    submit: &'static str,
    cancel: String,
    source: Option<PostTarget>,
    preview: Option<String>,
    errors: Vec<String>,
}

fn category_crumbs(category: &CategoryTarget) -> Vec<Crumb> {
    vec![Crumb {
        href: format!("/c/{}", category.category.slug),
        label: category.category.name.clone(),
    }]
}

fn thread_crumbs(thread: &ThreadTarget) -> Vec<Crumb> {
    let mut crumbs = match thread.category() {
        Some(category) => category_crumbs(category),
        None => vec![Crumb {
            href: "/messages".to_owned(),
            label: "Messages".to_owned(),
        }],
    };
    crumbs.push(Crumb {
        href: thread.href(),
        label: thread.name.clone(),
    });
    crumbs
}

/// A category's name with who can read it, as in "Lounge (members only)"
/// or "Planning (Organisers group)".
fn category_label(category: &CategoryTarget) -> String {
    match format::access_label(&category.read_level, &category.group_names) {
        None => category.category.name.clone(),
        // Group names keep their capitals.
        Some(access) if !category.group_names.is_empty() => {
            format!("{} ({access})", category.category.name)
        }
        Some(access) => format!("{} ({})", category.category.name, access.to_lowercase()),
    }
}

async fn render_compose(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    compose: Compose,
    draft: Draft,
    errors: Vec<String>,
    preview: bool,
) -> Result<Response, AppError> {
    // A preview is checked like a post, so that nothing too costly to
    // render is rendered, and rate limited.
    let mut status = status;
    let mut errors = errors;
    let preview = if preview {
        let body = writing::clean_body(&draft.body);
        let mut problems = Vec::new();
        writing::check_body(&body, &mut problems);
        if !problems.is_empty() {
            status = StatusCode::BAD_REQUEST;
            errors.extend(problems);
            None
        } else if !writing::may_preview(state, viewer) {
            status = StatusCode::TOO_MANY_REQUESTS;
            errors.push(TOO_FAST_PREVIEW.to_owned());
            None
        } else {
            Some(store::render_post(state.db(), &body).await?.html)
        }
    } else {
        None
    };
    let template = match compose {
        Compose::NewThread(category) => ComposeTemplate {
            heading: format!("New thread in {}", category.category.name),
            form_label: "New thread",
            action: format!("/c/{}/new", category.category.slug),
            title: Some(draft.title),
            categories: Vec::new(),
            reply_to: None,
            body: draft.body,
            body_label: "Opening post",
            submit: "Post thread",
            cancel: format!("/c/{}", category.category.slug),
            source: None,
            crumbs: category_crumbs(&category),
            layout: Layout::load(state, viewer, Nav::None).await?,
            preview,
            errors,
        },
        Compose::Reply(thread) => {
            let reply_to = match draft.reply_to {
                Some(id) => content::post_target(state.db(), viewer, id)
                    .await?
                    .filter(|post| post.thread.thread.id == thread.thread.id)
                    .map(|post| ReplyChoice {
                        post_id: post.post_id,
                        seq: post.seq,
                        username: post.author.username,
                    }),
                None => None,
            };
            ComposeTemplate {
                heading: format!("Reply to {}", thread.name.clone()),
                form_label: "Reply",
                action: format!("/t/{}/reply", thread.thread.id),
                title: None,
                categories: Vec::new(),
                reply_to,
                body: draft.body,
                body_label: "Your reply",
                submit: "Post reply",
                cancel: thread.href(),
                source: None,
                crumbs: thread_crumbs(&thread),
                layout: Layout::load(state, viewer, Nav::None).await?,
                preview,
                errors,
            }
        }
        Compose::Edit(post) => ComposeTemplate {
            heading: format!("Edit № {}", post.seq),
            form_label: "Edit post",
            action: format!("/p/{}/edit", post.post_id),
            title: None,
            categories: Vec::new(),
            reply_to: None,
            body: draft.body,
            body_label: "Post",
            submit: "Save changes",
            cancel: format!("/p/{}", post.post_id),
            source: None,
            crumbs: thread_crumbs(&post.thread),
            layout: Layout::load(state, viewer, Nav::None).await?,
            preview,
            errors,
        },
        Compose::Fork(source, destinations) => {
            let wanted = if draft.category.is_empty() {
                source
                    .thread
                    .category()
                    .map(|c| c.category.slug.clone())
                    .unwrap_or_default()
            } else {
                draft.category.clone()
            };
            ComposeTemplate {
                heading: format!("Start a new thread from № {}", source.seq),
                form_label: "New thread",
                action: format!("/p/{}/fork", source.post_id),
                title: Some(draft.title),
                categories: destinations
                    .iter()
                    .map(|c| CategoryOption {
                        slug: c.category.slug.clone(),
                        label: category_label(c),
                        selected: c.category.slug == wanted,
                    })
                    .collect(),
                reply_to: None,
                body: draft.body,
                body_label: "Your opening post",
                submit: "Create thread",
                cancel: format!("/p/{}", source.post_id),
                crumbs: thread_crumbs(&source.thread),
                source: Some(source),
                layout: Layout::load(state, viewer, Nav::None).await?,
                preview,
                errors,
            }
        }
    };
    render(status, &template)
}

/// What a member had typed when a write was refused.
enum Typed {
    /// Nothing: the request wasn't a form with text.
    Nothing,
    /// Text for a form that can be shown again.
    Form(Box<Compose>, Draft),
    /// Text for a form that can't be shown again, for example because the
    /// thread has been locked since.
    Text(Draft),
}

#[derive(Template)]
#[template(path = "draft_refused.html")]
struct DraftRefusedTemplate<'a> {
    layout: Layout,
    title: &'a str,
    problems: Vec<String>,
    draft_title: String,
    draft_body: String,
    back: &'a str,
    back_label: &'a str,
}

/// Shows what a refused write means: the form again with what was typed,
/// or a page explaining, with the text typed (if any) so it isn't lost.
async fn refused(
    state: &AppState,
    viewer: &Viewer,
    refusal: Refused,
    back_to: &str,
    typed: Typed,
) -> Result<Response, AppError> {
    let compose = match typed {
        Typed::Nothing => None,
        Typed::Form(compose, draft) => Some((*compose, draft)),
        Typed::Text(draft) if !draft.body.trim().is_empty() || !draft.title.trim().is_empty() => {
            let (status, title, problems) = match refusal {
                Refused::LogIn => return Ok(web::login_first(back_to)),
                Refused::NotFound => (
                    StatusCode::NOT_FOUND,
                    "Not found",
                    vec![
                        "What you were writing to isn’t there any more, or you can no longer see it."
                            .to_owned(),
                    ],
                ),
                Refused::NotAllowed(reason) => {
                    (StatusCode::FORBIDDEN, "Not allowed", vec![reason.to_owned()])
                }
                Refused::NotAllowedBecause(reason) => {
                    (StatusCode::FORBIDDEN, "Not allowed", vec![reason])
                }
                Refused::Invalid(problems) => {
                    (StatusCode::BAD_REQUEST, "That didn’t work", problems)
                }
                Refused::RateLimited => (
                    StatusCode::TOO_MANY_REQUESTS,
                    "Slow down",
                    vec![TOO_FAST.to_owned()],
                ),
            };
            return render(
                status,
                &DraftRefusedTemplate {
                    layout: Layout::load(state, viewer, Nav::None).await?,
                    title,
                    problems,
                    draft_title: draft.title,
                    draft_body: draft.body,
                    back: "/",
                    back_label: "Back to the forum index",
                },
            );
        }
        Typed::Text(_) => None,
    };
    match (refusal, compose) {
        (Refused::LogIn, _) => Ok(web::login_first(back_to)),
        (Refused::NotFound, _) => web::not_found(state, viewer).await,
        (Refused::NotAllowed(reason), _) => not_allowed(state, viewer, reason).await,
        (Refused::NotAllowedBecause(reason), _) => not_allowed(state, viewer, &reason).await,
        (Refused::Invalid(problems), Some((compose, draft))) => {
            render_compose(
                state,
                viewer,
                StatusCode::BAD_REQUEST,
                compose,
                draft,
                problems,
                false,
            )
            .await
        }
        (Refused::RateLimited, Some((compose, draft))) => {
            render_compose(
                state,
                viewer,
                StatusCode::TOO_MANY_REQUESTS,
                compose,
                draft,
                vec![TOO_FAST.to_owned()],
                false,
            )
            .await
        }
        (Refused::Invalid(problems), None) => {
            let problems: Vec<&str> = problems.iter().map(String::as_str).collect();
            web::message(
                state,
                viewer,
                StatusCode::BAD_REQUEST,
                "That didn’t work",
                &problems,
                Some((back_to, "Go back")),
            )
            .await
        }
        (Refused::RateLimited, None) => web::rate_limited(state, viewer).await,
    }
}

async fn posted(state: &AppState, viewer: &Viewer, posted: Posted) -> Result<Response, AppError> {
    Ok(see_other(&post_href(state, viewer, posted.post_id).await?))
}

// -- New threads

async fn new_thread_target(
    state: &AppState,
    viewer: &Viewer,
    slug: &str,
) -> Result<Result<CategoryTarget, Refused>, AppError> {
    let Some(category) = content::category_target(state.db(), viewer, slug).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if !writing::may_post_in(viewer, &category) {
        return Ok(Err(Refused::NotAllowed("You can’t post in this category.")));
    }
    Ok(Ok(category))
}

pub async fn new_thread_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/c/{slug}/new");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    match new_thread_target(&state, &viewer, &slug).await? {
        Ok(category) => {
            render_compose(
                &state,
                &viewer,
                StatusCode::OK,
                Compose::NewThread(category),
                Draft::default(),
                Vec::new(),
                false,
            )
            .await
        }
        Err(refusal) => refused(&state, &viewer, refusal, &back, Typed::Nothing).await,
    }
}

pub async fn new_thread(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/c/{slug}/new");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let draft = Draft::from_fields(&fields);
    if field(&fields, "preview").is_some() {
        return match new_thread_target(&state, &viewer, &slug).await? {
            Ok(category) => {
                render_compose(
                    &state,
                    &viewer,
                    StatusCode::OK,
                    Compose::NewThread(category),
                    draft,
                    Vec::new(),
                    true,
                )
                .await
            }
            Err(refusal) => refused(&state, &viewer, refusal, &back, Typed::Text(draft)).await,
        };
    }
    match writing::new_thread(&state, &viewer, &slug, &draft.title, &draft.body).await? {
        Ok(done) => posted(&state, &viewer, done).await,
        Err(refusal) => {
            let compose = match new_thread_target(&state, &viewer, &slug).await? {
                Ok(category) => Typed::Form(Box::new(Compose::NewThread(category)), draft),
                Err(_) => Typed::Text(draft),
            };
            refused(&state, &viewer, refusal, &back, compose).await
        }
    }
}

// -- Replies

async fn reply_target(
    state: &AppState,
    viewer: &Viewer,
    thread_id: i64,
) -> Result<Result<ThreadTarget, Refused>, AppError> {
    let Some(thread) = content::thread_target(state.db(), viewer, thread_id).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if let Some(reason) = writing::reply_refusal(state, viewer, &thread).await? {
        return Ok(Err(Refused::NotAllowed(reason)));
    }
    Ok(Ok(thread))
}

/// The text a quote starts a reply with.
fn quote_text(username: &str, markdown: &str) -> String {
    let mut text = format!("> @{username} wrote:\n");
    for line in markdown.lines() {
        if line.is_empty() {
            text.push_str(">\n");
        } else {
            text.push_str("> ");
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push('\n');
    text
}

pub async fn reply_page(
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
    let Some(thread_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let thread = match reply_target(&state, &viewer, thread_id).await? {
        Ok(thread) => thread,
        Err(refusal) => return refused(&state, &viewer, refusal, &back, Typed::Nothing).await,
    };
    let mut draft = Draft::default();
    let params = Query::<Fields>::try_from_uri(&uri)
        .map(|Query(params)| params)
        .unwrap_or_default();
    {
        let (quoting, wanted) = match (field(&params, "quote"), field(&params, "to")) {
            (Some(id), _) => (true, Some(id)),
            (None, Some(id)) => (false, Some(id)),
            (None, None) => (false, None),
        };
        if let Some(wanted) = wanted {
            let target = match parse_id(wanted) {
                Some(id) => content::post_target(state.db(), &viewer, id).await?,
                None => None,
            };
            // Only live posts in this thread can be quoted or replied to.
            let Some(target) =
                target.filter(|post| post.thread.thread.id == thread_id && post.is_live())
            else {
                return web::not_found(&state, &viewer).await;
            };
            if quoting && let Some(text) = &target.body {
                draft.body = quote_text(&target.author.username, &text.markdown);
            }
            draft.reply_to = Some(target.post_id);
        }
    }
    render_compose(
        &state,
        &viewer,
        StatusCode::OK,
        Compose::Reply(thread),
        draft,
        Vec::new(),
        false,
    )
    .await
}

pub async fn reply(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/t/{id}/reply");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let Some(thread_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let draft = Draft::from_fields(&fields);
    let bad_reply_to = field(&fields, "reply_to").is_some_and(|v| v.trim().parse::<i64>().is_err());
    if field(&fields, "preview").is_some() {
        return match reply_target(&state, &viewer, thread_id).await? {
            Ok(thread) => {
                render_compose(
                    &state,
                    &viewer,
                    StatusCode::OK,
                    Compose::Reply(thread),
                    draft,
                    Vec::new(),
                    true,
                )
                .await
            }
            Err(refusal) => refused(&state, &viewer, refusal, &back, Typed::Text(draft)).await,
        };
    }
    let outcome = if bad_reply_to {
        Err(Refused::Invalid(vec![
            "You can only reply to posts in this thread.".to_owned(),
        ]))
    } else {
        writing::reply(&state, &viewer, thread_id, &draft.body, draft.reply_to).await?
    };
    match outcome {
        Ok(done) => posted(&state, &viewer, done).await,
        Err(refusal) => {
            let compose = match reply_target(&state, &viewer, thread_id).await? {
                Ok(thread) => Typed::Form(Box::new(Compose::Reply(thread)), draft),
                Err(other) => {
                    return refused(&state, &viewer, other, &back, Typed::Text(draft)).await;
                }
            };
            refused(&state, &viewer, refusal, &back, compose).await
        }
    }
}

// -- Editing

/// A live post the viewer may edit.
async fn edit_target(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
) -> Result<Result<PostTarget, Refused>, AppError> {
    let Some(post) = content::post_target(state.db(), viewer, post_id)
        .await?
        .filter(PostTarget::is_live)
    else {
        return Ok(Err(Refused::NotFound));
    };
    if !writing::may_edit(viewer, &post) {
        return Ok(Err(Refused::NotAllowed(if post.thread.locked {
            "This thread is locked."
        } else {
            "You can only edit your own posts."
        })));
    }
    Ok(Ok(post))
}

pub async fn edit_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/p/{id}/edit");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match edit_target(&state, &viewer, post_id).await? {
        Ok(post) => {
            let draft = Draft {
                body: post
                    .body
                    .as_ref()
                    .map(|b| b.markdown.clone())
                    .unwrap_or_default(),
                ..Draft::default()
            };
            render_compose(
                &state,
                &viewer,
                StatusCode::OK,
                Compose::Edit(post),
                draft,
                Vec::new(),
                false,
            )
            .await
        }
        Err(refusal) => refused(&state, &viewer, refusal, &back, Typed::Nothing).await,
    }
}

pub async fn edit(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/p/{id}/edit");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let draft = Draft::from_fields(&fields);
    if field(&fields, "preview").is_some() {
        return match edit_target(&state, &viewer, post_id).await? {
            Ok(post) => {
                render_compose(
                    &state,
                    &viewer,
                    StatusCode::OK,
                    Compose::Edit(post),
                    draft,
                    Vec::new(),
                    true,
                )
                .await
            }
            Err(refusal) => refused(&state, &viewer, refusal, &back, Typed::Text(draft)).await,
        };
    }
    match writing::edit(&state, &viewer, post_id, &draft.body).await? {
        Ok(done) => posted(&state, &viewer, done).await,
        Err(refusal) => {
            let compose = match edit_target(&state, &viewer, post_id).await? {
                Ok(post) => Typed::Form(Box::new(Compose::Edit(post)), draft),
                Err(_) => Typed::Text(draft),
            };
            refused(&state, &viewer, refusal, &back, compose).await
        }
    }
}

// -- Deleting

#[derive(Template)]
#[template(path = "delete_post.html")]
struct DeleteTemplate {
    layout: Layout,
    post: PostTarget,
}

pub async fn delete_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/p/{id}/delete");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(post) = content::post_target(state.db(), &viewer, post_id)
        .await?
        .filter(PostTarget::is_live)
    else {
        return web::not_found(&state, &viewer).await;
    };
    if !writing::may_delete(&viewer, &post) {
        return not_allowed(&state, &viewer, "You can only delete your own posts.").await;
    }
    render(
        StatusCode::OK,
        &DeleteTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            post,
        },
    )
}

pub async fn delete(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/p/{id}/delete");
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match writing::delete(&state, &viewer, post_id).await? {
        Ok(done) => posted(&state, &viewer, done).await,
        Err(refusal) => refused(&state, &viewer, refusal, &back, Typed::Nothing).await,
    }
}

// -- Edit history

#[derive(Template)]
#[template(path = "revisions.html")]
struct RevisionsTemplate {
    layout: Layout,
    history: PostHistory,
    /// The number of the version shown.
    selected: i64,
    /// What it is compared with, and how.
    compared_with: Option<i64>,
    note: String,
    /// The selected version's text, as changes from the one before.
    segments: Vec<Segment>,
    redacted: bool,
}

impl RevisionsTemplate {
    fn version_href(&self, number: &i64) -> String {
        let latest = self.history.versions.len() as i64;
        if *number == latest {
            format!("/p/{}/revisions", self.history.post.post_id)
        } else {
            format!("/p/{}/revisions?v={number}", self.history.post.post_id)
        }
    }
}

/// The `?v=` number the request asks for: `Ok(None)` if there is none, or
/// `Err` if the query isn't exactly one positive `v`.
fn requested_version(uri: &Uri) -> Result<Option<i64>, ()> {
    let Ok(Query(params)) = Query::<Vec<(String, String)>>::try_from_uri(uri) else {
        return Err(());
    };
    match params.as_slice() {
        [] => Ok(None),
        [(key, value)] if key == "v" => match value.parse::<i64>() {
            Ok(n) if n >= 1 && value == &n.to_string() => Ok(Some(n)),
            _ => Err(()),
        },
        _ => Err(()),
    }
}

pub async fn revisions(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Response, AppError> {
    let (Some(post_id), Ok(wanted)) = (parse_id(&id), requested_version(&uri)) else {
        return web::not_found(&state, &viewer).await;
    };
    let mut deps = Deps::new();
    let Some(history) = content::post_history(state.db(), &viewer, post_id, &mut deps).await?
    else {
        return web::not_found(&state, &viewer).await;
    };
    let latest = history.versions.len() as i64;
    // The newest version has the bare URL. Only for now, though: after
    // another edit this number means an older version.
    if wanted == Some(latest) {
        return Ok(Redirect::temporary(&format!("/p/{post_id}/revisions")).into_response());
    }
    let selected = wanted.unwrap_or(latest);
    if selected > latest {
        return web::not_found(&state, &viewer).await;
    }
    let index = (selected - 1) as usize;
    let version = &history.versions[index];
    // Compare with the newest earlier version that hasn't been redacted.
    let earlier = history.versions[..index]
        .iter()
        .rev()
        .find(|v| v.markdown.is_some());
    let skipped: Vec<i64> = history.versions[..index]
        .iter()
        .rev()
        .take_while(|v| v.markdown.is_none())
        .map(|v| v.number)
        .collect();
    let (segments, compared_with, note) = match (&version.markdown, earlier) {
        (None, _) => (Vec::new(), None, String::new()),
        (Some(text), None) if selected == 1 => (
            vec![Segment::Same(text.clone())],
            None,
            "The original post".to_owned(),
        ),
        (Some(text), None) => (
            vec![Segment::Same(text.clone())],
            None,
            "Earlier versions were redacted".to_owned(),
        ),
        (Some(text), Some(before)) => {
            let mut note = format!("Compared with version {}", before.number);
            if !skipped.is_empty() {
                let list: Vec<String> = skipped.iter().rev().map(i64::to_string).collect();
                note.push_str(&format!(
                    " (version {} {} redacted)",
                    list.join(", "),
                    if skipped.len() == 1 { "is" } else { "are" }
                ));
            }
            (
                diff::words(before.markdown.as_deref().unwrap_or_default(), text),
                Some(before.number),
                note,
            )
        }
    };
    let redacted = version.markdown.is_none();
    cacheable(
        &RevisionsTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            history,
            selected,
            compared_with,
            note,
            segments,
            redacted,
        },
        &deps,
    )
}

// -- Reactions

#[derive(Template)]
#[template(path = "reactions.html")]
struct ReactionsTemplate {
    layout: Layout,
    reactions: PostReactions,
    logged_in: bool,
}

pub async fn reactions_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Response, AppError> {
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    if let Some(redirect) = web::redirect_unless_canonical(&uri, &format!("/p/{post_id}/reactions"))
    {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let Some(reactions) = content::post_reactions(state.db(), &viewer, post_id, &mut deps).await?
    else {
        return web::not_found(&state, &viewer).await;
    };
    cacheable(
        &ReactionsTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            reactions,
            logged_in: viewer.user().is_some(),
        },
        &deps,
    )
}

pub async fn react(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/p/{id}/reactions");
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let emoji = field(&fields, "emoji").unwrap_or_default();
    let add = field(&fields, "action") != Some("remove");
    match writing::react(&state, &viewer, post_id, emoji, add).await? {
        Ok(done) => posted(&state, &viewer, done).await,
        Err(refusal) => refused(&state, &viewer, refusal, &back, Typed::Nothing).await,
    }
}

// -- Quote-forks

async fn fork_target(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
) -> Result<Option<(PostTarget, Vec<CategoryTarget>)>, AppError> {
    let Some(source) = content::post_target(state.db(), viewer, post_id)
        .await?
        .filter(PostTarget::is_live)
    else {
        return Ok(None);
    };
    let destinations = writing::fork_destinations(state, viewer, &source).await?;
    Ok(Some((source, destinations)))
}

pub async fn fork_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/p/{id}/fork");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let Some((source, destinations)) = fork_target(&state, &viewer, post_id).await? else {
        return web::not_found(&state, &viewer).await;
    };
    if destinations.is_empty() {
        return not_allowed(
            &state,
            &viewer,
            "There’s no category you can post in that is at least as private as this post’s.",
        )
        .await;
    }
    render_compose(
        &state,
        &viewer,
        StatusCode::OK,
        Compose::Fork(source, destinations),
        Draft::default(),
        Vec::new(),
        false,
    )
    .await
}

pub async fn fork(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/p/{id}/fork");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let draft = Draft::from_fields(&fields);
    if field(&fields, "preview").is_some() {
        let Some((source, destinations)) = fork_target(&state, &viewer, post_id).await? else {
            return refused(
                &state,
                &viewer,
                Refused::NotFound,
                &back,
                Typed::Text(draft),
            )
            .await;
        };
        return render_compose(
            &state,
            &viewer,
            StatusCode::OK,
            Compose::Fork(source, destinations),
            draft,
            Vec::new(),
            true,
        )
        .await;
    }
    match writing::fork(
        &state,
        &viewer,
        post_id,
        &draft.category,
        &draft.title,
        &draft.body,
    )
    .await?
    {
        Ok(done) => posted(&state, &viewer, done).await,
        Err(refusal) => {
            let compose = match fork_target(&state, &viewer, post_id).await? {
                Some((source, destinations)) => {
                    Typed::Form(Box::new(Compose::Fork(source, destinations)), draft)
                }
                None => Typed::Text(draft),
            };
            refused(&state, &viewer, refusal, &back, compose).await
        }
    }
}

// -- Read markers

pub async fn mark_category_read(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/c/{slug}");
    match writing::mark_category_read(&state, &viewer, &slug).await? {
        Ok(category) => Ok(see_other(&format!("/c/{}", category.category.slug))),
        Err(refusal) => refused(&state, &viewer, refusal, &back, Typed::Nothing).await,
    }
}

// ---------------------------------------------------------------------------
// The admin's reactions

#[derive(Template)]
#[template(path = "admin_reactions.html")]
struct AdminReactionsTemplate {
    layout: Layout,
    types: Vec<ReactionType>,
    max: usize,
    errors: Vec<String>,
}

async fn render_admin_reactions(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    render(
        status,
        &AdminReactionsTemplate {
            layout: Layout::load(state, viewer, Nav::Admin).await?,
            types: content::reaction_types(state.db(), false).await?,
            max: MAX_REACTIONS,
            errors,
        },
    )
}

pub async fn admin_reactions_page(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/admin/reactions"));
    }
    if !viewer.is_admin() {
        return web::forbidden(&state, &viewer).await;
    }
    render_admin_reactions(&state, &viewer, StatusCode::OK, Vec::new()).await
}

pub async fn admin_reactions(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first("/admin/reactions"));
    }
    if !viewer.is_admin() {
        return web::forbidden(&state, &viewer).await;
    }
    let emoji = field(&fields, "emoji").unwrap_or_default().trim();
    let action = field(&fields, "action").unwrap_or_default();
    match store::change_reaction_type(state.db(), action, emoji, MAX_REACTIONS).await? {
        Ok(()) => {
            state.changed(Change::SiteChanged).await?;
            Ok(see_other("/admin/reactions"))
        }
        Err(problem) => {
            render_admin_reactions(&state, &viewer, StatusCode::BAD_REQUEST, vec![problem]).await
        }
    }
}
