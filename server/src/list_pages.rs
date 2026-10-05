//! A member's lists (DESIGN.md §3.3), from their settings:
//!
//! - `/settings/lists`: the lists they own, a form to make another, and
//!   the lists they are on, with whose each is and who else is on it;
//! - `/settings/lists/{id}`: one list. Its owner renames it, adds people
//!   by username, takes them off, and deletes it if no space's grants name
//!   it; anyone on it sees who else is.
//!
//! A list is hidden from everyone who isn't on it: for them, its page and
//! forms don't exist (404), whoever they are. People on a list who don't
//! own it may look but not change it (403). Nobody is told they were put
//! on a list.
//!
//! These pages are only for logged-in members and never cached; what a
//! list lets people read is decided afresh on every request, so changing
//! one clears nothing.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use crate::AppState;
use crate::admin_pages::DoneQuery;
use crate::content::{self, SpaceRef};
use crate::error::AppError;
use crate::lists::{self, Change, List};
use crate::pages::parse_id;
use crate::ratelimit::{Action, Scope};
use crate::viewer::{UserCtx, Viewer};
use crate::web::{self, Layout, Nav, render, see_other};

/// What the list-making form says, in DESIGN.md's words.
pub const CREATION_NOTE: &str = "People on this list can see its name and who else is on it.";

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NameForm {
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct MemberForm {
    username: String,
}

// ---------------------------------------------------------------------------
// Every list

#[derive(Template)]
#[template(path = "lists.html")]
struct ListsTemplate {
    layout: Layout,
    me: i64,
    notice: Option<&'static str>,
    errors: Vec<String>,
    own: Vec<List>,
    others: Vec<List>,
    /// The name typed into the form for a new list, if it was refused.
    name: String,
}

impl ListsTemplate {
    fn creation_note(&self) -> &'static str {
        CREATION_NOTE
    }
}

async fn render_lists(
    state: &AppState,
    viewer: &Viewer,
    user: &UserCtx,
    status: StatusCode,
    notice: Option<&'static str>,
    errors: Vec<String>,
    name: String,
) -> Result<Response, AppError> {
    let (own, others) = lists::known_to(state.db(), user.id)
        .await?
        .into_iter()
        .partition(|list| list.yours);
    render(
        status,
        &ListsTemplate {
            layout: Layout::load(state, viewer, Nav::Account).await?,
            me: user.id,
            notice,
            errors,
            own,
            others,
            name,
        },
    )
}

pub async fn lists_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first("/settings/lists"));
    };
    let notice = match query.done.as_str() {
        "deleted" => Some("The list is deleted."),
        _ => None,
    };
    render_lists(
        &state,
        &viewer,
        user,
        StatusCode::OK,
        notice,
        Vec::new(),
        String::new(),
    )
    .await
}

/// The member who may change their lists now, or the page for whoever
/// may not: visitors log in, and suspended members change nothing.
async fn writer<'v>(
    state: &AppState,
    viewer: &'v Viewer,
    back_to: &str,
) -> Result<Result<&'v UserCtx, Response>, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(Err(web::login_first(back_to)));
    };
    if !user.may_act() {
        return Ok(Err(web::forbidden(state, viewer).await?));
    }
    Ok(Ok(user))
}

pub async fn create_list(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<NameForm>,
) -> Result<Response, AppError> {
    let user = match writer(&state, &viewer, "/settings/lists").await? {
        Ok(user) => user,
        Err(response) => return Ok(response),
    };
    if let Err(why) = lists::clean_name(&form.name) {
        return render_lists(
            &state,
            &viewer,
            user,
            StatusCode::BAD_REQUEST,
            None,
            vec![why],
            form.name,
        )
        .await;
    }
    if !state
        .limiter()
        .check(Action::CreateList, Scope::User, &user.id.to_string())
    {
        return web::rate_limited(&state, &viewer).await;
    }
    match lists::create(state.db(), user.id, &form.name).await? {
        Ok(id) => Ok(see_other(&format!("/settings/lists/{id}?done=created"))),
        Err(why) => {
            render_lists(
                &state,
                &viewer,
                user,
                StatusCode::BAD_REQUEST,
                None,
                vec![why],
                form.name,
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// One list

#[derive(Template)]
#[template(path = "list.html")]
struct ListTemplate {
    layout: Layout,
    notice: Option<&'static str>,
    errors: Vec<String>,
    list: List,
    /// The spaces the viewer can read that name the list.
    spaces: Vec<SpaceRef>,
    /// How many spaces name it, readable by the viewer or not: only a list
    /// none name can be deleted.
    named_in: i64,
    /// How many threads' own restrictions name it (DESIGN.md §3.4).
    threads_naming: i64,
    /// The threads the viewer can read whose restriction names it.
    threads: Vec<content::ThreadRef>,
    /// The name and username typed into the forms, if refused.
    name: String,
    username: String,
}

impl ListTemplate {
    fn title(&self) -> String {
        if self.list.yours {
            self.list.name.clone()
        } else {
            format!("{}’s {}", self.list.owner.display_name, self.list.name)
        }
    }

    fn named_in(&self) -> String {
        lists::named_in(self.named_in, self.threads_naming)
    }

    /// Whether nothing names it, so that it can be deleted.
    fn deletable(&self) -> bool {
        self.named_in == 0 && self.threads_naming == 0
    }

    /// For its owner, how many spaces they can't read name the list. Only
    /// its owner is told: to anyone else on it, a space they can't read
    /// might as well not exist.
    fn unreadable_spaces(&self) -> i64 {
        if self.list.yours {
            self.named_in - self.spaces.len() as i64
        } else {
            0
        }
    }

    /// For its owner, how many threads they can't read name the list.
    fn unreadable_threads(&self) -> i64 {
        if self.list.yours {
            self.threads_naming - self.threads.len() as i64
        } else {
            0
        }
    }

    fn unreadable_threads_note(&self) -> String {
        let too = if self.spaces.is_empty() && self.threads.is_empty() {
            ""
        } else {
            " too"
        };
        match self.unreadable_threads() {
            1 => format!(
                "A thread you can’t read names it{too}. Its author and the people who run its space can change who may take part in it."
            ),
            n => format!(
                "{n} threads you can’t read name it{too}. Their authors and the people who run their spaces can change who may take part in them."
            ),
        }
    }

    /// Whether the panel lists nothing at all.
    fn named_nowhere_shown(&self) -> bool {
        self.spaces.is_empty()
            && self.threads.is_empty()
            && self.unreadable_spaces() == 0
            && self.unreadable_threads() == 0
    }

    fn unreadable_note(&self) -> String {
        let too = if self.spaces.is_empty() { "" } else { " too" };
        match self.unreadable_spaces() {
            1 => format!(
                "A space you can’t read names it{too}. Only that space’s administrators can take the list out of its settings."
            ),
            n => format!(
                "{n} spaces you can’t read name it{too}. Only their administrators can take the list out of their settings."
            ),
        }
    }
}

/// What to show on a list's page besides the list.
#[derive(Default)]
struct Outcome {
    notice: Option<&'static str>,
    errors: Vec<String>,
    name: Option<String>,
    username: String,
}

async fn render_list(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    list: List,
    outcome: Outcome,
) -> Result<Response, AppError> {
    let db = state.db();
    render(
        status,
        &ListTemplate {
            layout: Layout::load(state, viewer, Nav::Account).await?,
            spaces: content::spaces_naming(db, viewer, list.id).await?,
            named_in: lists::spaces_naming(db, list.id).await?,
            threads_naming: lists::threads_naming(db, list.id).await?,
            threads: content::threads_naming(db, viewer, list.id).await?,
            notice: outcome.notice,
            errors: outcome.errors,
            name: outcome.name.unwrap_or_else(|| list.name.clone()),
            username: outcome.username,
            list,
        },
    )
}

/// The list at `id` as `user` knows it, or the page for one they don't:
/// as far as they can tell, there is no such list.
async fn find(
    state: &AppState,
    viewer: &Viewer,
    user: &UserCtx,
    id: &str,
) -> Result<Result<List, Response>, AppError> {
    let list = match parse_id(id) {
        Some(id) => lists::known(state.db(), user.id, id).await?,
        None => None,
    };
    match list {
        Some(list) => Ok(Ok(list)),
        None => Ok(Err(web::not_found(state, viewer).await?)),
    }
}

/// [`find`], for changing the list: only its owner may.
async fn find_own(
    state: &AppState,
    viewer: &Viewer,
    id: &str,
) -> Result<Result<(i64, List), Response>, AppError> {
    let user = match writer(state, viewer, &format!("/settings/lists/{id}")).await? {
        Ok(user) => user,
        Err(response) => return Ok(Err(response)),
    };
    let list = match find(state, viewer, user, id).await? {
        Ok(list) => list,
        Err(response) => return Ok(Err(response)),
    };
    if !list.yours || list.frozen() {
        return Ok(Err(web::forbidden(state, viewer).await?));
    }
    Ok(Ok((user.id, list)))
}

pub async fn list_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(web::login_first(&format!("/settings/lists/{id}")));
    };
    let list = match find(&state, &viewer, user, &id).await? {
        Ok(list) => list,
        Err(response) => return Ok(response),
    };
    let notice = match query.done.as_str() {
        "created" => Some("The list is ready. Add people to it by username."),
        "saved" => Some("Saved."),
        "added" => Some("Added to the list."),
        "removed" => Some("Taken off the list."),
        _ => None,
    };
    let outcome = Outcome {
        notice,
        ..Outcome::default()
    };
    render_list(&state, &viewer, StatusCode::OK, list, outcome).await
}

/// Where a change to a list leads: back to its page saying `done`, or
/// the page again with what went wrong.
async fn after(
    state: &AppState,
    viewer: &Viewer,
    list: List,
    change: Change,
    done: &str,
    outcome: Outcome,
) -> Result<Response, AppError> {
    match change {
        Change::Done => Ok(see_other(&format!(
            "/settings/lists/{}?done={done}",
            list.id
        ))),
        Change::NotFound => web::not_found(state, viewer).await,
        Change::Refused(why) => {
            let outcome = Outcome {
                errors: vec![why],
                ..outcome
            };
            render_list(state, viewer, StatusCode::BAD_REQUEST, list, outcome).await
        }
    }
}

pub async fn rename_list(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<NameForm>,
) -> Result<Response, AppError> {
    let (owner, list) = match find_own(&state, &viewer, &id).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let change = lists::rename(state.db(), owner, list.id, &form.name).await?;
    let outcome = Outcome {
        name: Some(form.name),
        ..Outcome::default()
    };
    after(&state, &viewer, list, change, "saved", outcome).await
}

pub async fn add_member(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<MemberForm>,
) -> Result<Response, AppError> {
    let (owner, list) = match find_own(&state, &viewer, &id).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let change = lists::add_member(state.db(), owner, list.id, &form.username).await?;
    let outcome = Outcome {
        username: form.username,
        ..Outcome::default()
    };
    after(&state, &viewer, list, change, "added", outcome).await
}

pub async fn remove_member(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((id, user_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let (owner, list) = match find_own(&state, &viewer, &id).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let Some(user_id) = parse_id(&user_id) else {
        return web::not_found(&state, &viewer).await;
    };
    let change = lists::remove_member(state.db(), owner, list.id, user_id).await?;
    after(&state, &viewer, list, change, "removed", Outcome::default()).await
}

pub async fn delete_list(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let (owner, list) = match find_own(&state, &viewer, &id).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    match lists::delete(state.db(), owner, list.id).await? {
        Change::Done => Ok(see_other("/settings/lists?done=deleted")),
        change => after(&state, &viewer, list, change, "", Outcome::default()).await,
    }
}
