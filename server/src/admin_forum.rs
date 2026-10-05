//! Admin pages for the shape of the forum (DESIGN.md §3.1, §3.5), for
//! admins only:
//!
//! - `/admin/spaces`: every space, with who can read and post in each,
//!   leading to each space's settings and to the editor of the default
//!   front page (`/admin/layout`, in `layout_pages`);
//! - `/admin/spaces/new` and `/admin/spaces/{id}`, which lead to the
//!   space's own pages (`space_pages`);
//! - `/admin/groups` and `/admin/groups/{id}`: site groups and their
//!   members. Members' own lists (DESIGN.md §3.3) are hidden from admins
//!   as from everyone not on them, so these pages know nothing of them.
//!
//! Group changes matter only to logged-in members, whose pages are never
//! cached.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use crate::AppState;
use crate::admin_pages::{DoneQuery, admin, not_admin};
use crate::content::{self, SpaceTarget};
use crate::error::AppError;
use crate::format;
use crate::groups::{self, GroupDetail, GroupFields, GroupSummary};
use crate::pages::parse_id;
use crate::users;
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, render, see_other};

/// Sends away anyone who isn't an admin.
macro_rules! admins_only {
    ($state:expr, $viewer:expr, $path:expr) => {
        if admin(&$viewer).is_none() {
            return not_admin(&$state, &$viewer, $path).await;
        }
    };
}

fn notice_for(done: &str) -> Option<&'static str> {
    match done {
        "saved" => Some("Saved."),
        "deleted" => Some("Deleted."),
        "added" => Some("Added to the group."),
        "removed" => Some("Taken out of the group."),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Every space, with who can read and post in each

#[derive(Template)]
#[template(path = "admin_spaces.html")]
struct SpacesTemplate {
    layout: Layout,
    notice: Option<&'static str>,
    spaces: Vec<SpaceTarget>,
}

impl SpacesTemplate {
    fn access(&self, space: &SpaceTarget) -> String {
        format::access_label(space.read_level(), &space.group_names, &space.list_names)
            .unwrap_or_else(|| "Everyone".to_owned())
    }

    fn posting(&self, space: &SpaceTarget) -> &'static str {
        format::grant_label(space.rules.start.level)
    }
}

pub async fn spaces_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/spaces");
    render(
        StatusCode::OK,
        &SpacesTemplate {
            layout: Layout::load(&state, &viewer, Nav::Admin).await?,
            notice: notice_for(&query.done),
            // Feeds without threads are left out (DESIGN.md §3.2).
            // Staff run every space, readable to them or not.
            spaces: content::administered_spaces(state.db(), &viewer)
                .await?
                .into_iter()
                .filter(|c| c.listed())
                .collect(),
        },
    )
}

// ---------------------------------------------------------------------------
// One space: its settings live with the space itself (`/c/{slug}/settings`,
// in `space_pages`). The v0.1 addresses lead there.

pub async fn new_space_page(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/spaces/new");
    Ok(see_other("/spaces/new"))
}

pub async fn space_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/spaces");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(space) = content::administered_space_by_id(state.db(), &viewer, id).await? else {
        return web::not_found(&state, &viewer).await;
    };
    Ok(see_other(&format!("/c/{}/settings", space.space.slug)))
}

// ---------------------------------------------------------------------------
// Groups

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct GroupForm {
    name: String,
    slug: String,
    description: String,
}

impl GroupForm {
    fn fields(&self) -> GroupFields {
        GroupFields {
            slug: self.slug.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
        }
    }
}

#[derive(Template)]
#[template(path = "admin_groups.html")]
struct GroupsTemplate {
    layout: Layout,
    notice: Option<&'static str>,
    errors: Vec<String>,
    groups: Vec<GroupSummary>,
    form: GroupForm,
}

async fn render_groups(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    notice: Option<&'static str>,
    errors: Vec<String>,
    form: GroupForm,
) -> Result<Response, AppError> {
    render(
        status,
        &GroupsTemplate {
            layout: Layout::load(state, viewer, Nav::Admin).await?,
            notice,
            errors,
            groups: groups::list(state.db()).await?,
            form,
        },
    )
}

pub async fn groups_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/groups");
    render_groups(
        &state,
        &viewer,
        StatusCode::OK,
        notice_for(&query.done),
        Vec::new(),
        GroupForm::default(),
    )
    .await
}

pub async fn create_group(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<GroupForm>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/groups");
    let created = match form.fields().clean() {
        Err(problems) => Err(problems),
        Ok(fields) => groups::create(state.db(), &fields)
            .await?
            .map_err(|why| vec![why]),
    };
    match created {
        Ok(id) => Ok(see_other(&format!("/admin/groups/{id}"))),
        Err(problems) => {
            render_groups(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                None,
                problems,
                form,
            )
            .await
        }
    }
}

#[derive(Template)]
#[template(path = "admin_group.html")]
struct GroupTemplate {
    layout: Layout,
    notice: Option<&'static str>,
    errors: Vec<String>,
    detail: GroupDetail,
    form: GroupForm,
    username: String,
}

async fn render_group(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    detail: GroupDetail,
    outcome: GroupOutcome,
) -> Result<Response, AppError> {
    let form = outcome.form.unwrap_or_else(|| GroupForm {
        name: detail.group.name.clone(),
        slug: detail.group.slug.clone(),
        description: detail.group.description.clone(),
    });
    render(
        status,
        &GroupTemplate {
            layout: Layout::load(state, viewer, Nav::Admin).await?,
            notice: outcome.notice,
            errors: outcome.errors,
            detail,
            form,
            username: outcome.username,
        },
    )
}

/// What to show on a group's page besides the group.
#[derive(Default)]
struct GroupOutcome {
    notice: Option<&'static str>,
    errors: Vec<String>,
    /// The details form as typed, if it was refused.
    form: Option<GroupForm>,
    /// The username typed into the add-member form, if it was refused.
    username: String,
}

/// The group with this id (from a path), or the response to send instead.
async fn find_group(
    state: &AppState,
    viewer: &Viewer,
    id: &str,
) -> Result<Result<GroupDetail, Response>, AppError> {
    let detail = match parse_id(id) {
        Some(id) => groups::detail(state.db(), id).await?,
        None => None,
    };
    match detail {
        Some(detail) => Ok(Ok(detail)),
        None => Ok(Err(web::not_found(state, viewer).await?)),
    }
}

pub async fn group_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/groups");
    let detail = match find_group(&state, &viewer, &id).await? {
        Ok(detail) => detail,
        Err(response) => return Ok(response),
    };
    let outcome = GroupOutcome {
        notice: notice_for(&query.done),
        ..GroupOutcome::default()
    };
    render_group(&state, &viewer, StatusCode::OK, detail, outcome).await
}

pub async fn update_group(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<GroupForm>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/groups");
    let detail = match find_group(&state, &viewer, &id).await? {
        Ok(detail) => detail,
        Err(response) => return Ok(response),
    };
    let id = detail.group.id;
    let updated = match form.fields().clean() {
        Err(problems) => Err(problems),
        Ok(fields) => groups::update(state.db(), id, &fields)
            .await?
            .map_err(|why| vec![why]),
    };
    match updated {
        Ok(false) => web::not_found(&state, &viewer).await,
        Ok(true) => Ok(see_other(&format!("/admin/groups/{id}?done=saved"))),
        Err(errors) => {
            let outcome = GroupOutcome {
                errors,
                form: Some(form),
                ..GroupOutcome::default()
            };
            render_group(&state, &viewer, StatusCode::BAD_REQUEST, detail, outcome).await
        }
    }
}

#[derive(Template)]
#[template(path = "admin_delete_group.html")]
struct DeleteGroupTemplate {
    layout: Layout,
    detail: GroupDetail,
    /// Why it wasn't deleted, after trying.
    problem: Option<String>,
}

/// Asks before deleting a group, saying who and what it will affect.
pub async fn delete_group_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/groups");
    let detail = match find_group(&state, &viewer, &id).await? {
        Ok(detail) => detail,
        Err(response) => return Ok(response),
    };
    render(
        StatusCode::OK,
        &DeleteGroupTemplate {
            layout: Layout::load(&state, &viewer, Nav::Admin).await?,
            detail,
            problem: None,
        },
    )
}

pub async fn delete_group(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/groups");
    let detail = match find_group(&state, &viewer, &id).await? {
        Ok(detail) => detail,
        Err(response) => return Ok(response),
    };
    match groups::delete(state.db(), detail.group.id).await? {
        Ok(true) => Ok(see_other("/admin/groups?done=deleted")),
        Ok(false) => web::not_found(&state, &viewer).await,
        Err(why) => render(
            StatusCode::BAD_REQUEST,
            &DeleteGroupTemplate {
                layout: Layout::load(&state, &viewer, Nav::Admin).await?,
                detail,
                problem: Some(why),
            },
        ),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MemberForm {
    username: String,
}

pub async fn add_member(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    let Some(user) = admin(&viewer) else {
        return not_admin(&state, &viewer, "/admin/groups").await;
    };
    let form = MemberForm {
        username: fields
            .iter()
            .find(|(key, _)| key == "username")
            .map(|(_, value)| value.clone())
            .unwrap_or_default(),
    };
    let added_by = user.id;
    let detail = match find_group(&state, &viewer, &id).await? {
        Ok(detail) => detail,
        Err(response) => return Ok(response),
    };
    let id = detail.group.id;
    let username = form.username.trim().trim_start_matches('@');
    match users::id_by_username(state.db(), username).await? {
        Some(user_id) => {
            // Joining a group that lets them read what they can't now is
            // breaking glass on each thing it opens (DESIGN.md §8), which
            // they confirm, saying why; those told are those who could
            // read each before.
            let mut glass = None;
            if user_id == user.id {
                let opens = content::access::opened_by_joining(state.db(), &viewer, id).await?;
                if !opens.is_empty() {
                    let action = format!("/admin/groups/{id}/members");
                    match crate::glass_pages::ask_change(
                        &state,
                        &viewer,
                        opens.clone(),
                        crate::glass_pages::Change {
                            action: &action,
                            fields: &fields,
                            says: format!(
                                "Joining {} would let you read what you can’t now.",
                                detail.group.name
                            ),
                            button: "Break glass and join",
                            back: &format!("/admin/groups/{id}"),
                            by_change: true,
                        },
                    )
                    .await?
                    {
                        crate::glass_pages::Confirmation::Ask(response) => return Ok(response),
                        crate::glass_pages::Confirmation::Given(reason) => {
                            glass =
                                Some((reason, crate::glass::readers(&state, user, &opens).await?));
                        }
                    }
                }
            }
            // Breaking glass, they join with its grants and notices or not
            // at all (DESIGN.md §8).
            let mut tx = state.db().begin().await?;
            groups::add_member(&mut tx, id, user_id, Some(added_by)).await?;
            let recorded = match glass {
                Some((reason, readers)) => {
                    Some(crate::glass::record(&mut tx, user, &readers, &reason, None, true).await?)
                }
                None => None,
            };
            tx.commit().await?;
            if let Some(recorded) = recorded {
                recorded.announce(&state).await?;
            }
            Ok(see_other(&format!("/admin/groups/{id}?done=added")))
        }
        None => {
            let outcome = GroupOutcome {
                errors: vec![format!("There is no member called “{username}”.")],
                username: form.username.clone(),
                ..GroupOutcome::default()
            };
            render_group(&state, &viewer, StatusCode::BAD_REQUEST, detail, outcome).await
        }
    }
}

pub async fn remove_member(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((id, user_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/groups");
    // Only site groups: members' lists are theirs alone.
    let detail = match find_group(&state, &viewer, &id).await? {
        Ok(detail) => detail,
        Err(response) => return Ok(response),
    };
    let id = detail.group.id;
    let Some(user_id) = parse_id(&user_id) else {
        return web::not_found(&state, &viewer).await;
    };
    if !groups::remove_member(state.db(), id, user_id).await? {
        return web::not_found(&state, &viewer).await;
    }
    Ok(see_other(&format!("/admin/groups/{id}?done=removed")))
}
