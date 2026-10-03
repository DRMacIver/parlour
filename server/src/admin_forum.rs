//! Admin pages for the shape of the forum (DESIGN.md §3.1), for admins only:
//!
//! - `/admin/spaces`: the sections on the index and the spaces in
//!   them, with who can read and post in each;
//! - `/admin/spaces/new` and `/admin/spaces/{id}`, which lead to the
//!   space's own pages (`space_pages`);
//! - `/admin/groups` and `/admin/groups/{id}`: groups and their members.
//!
//! Changing who can read a space clears every cached page that could
//! show its threads; group changes matter only to logged-in members, whose
//! pages are never cached.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use crate::AppState;
use crate::admin_pages::{DoneQuery, admin, not_admin};
use crate::changes::Change;
use crate::content::{self, Section, SpaceTarget};
use crate::error::AppError;
use crate::format;
use crate::groups::{self, GroupDetail, GroupFields, GroupSummary};
use crate::pages::parse_id;
use crate::store;
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
        "section" => Some("Section saved."),
        "section-deleted" => Some("Section deleted. Its spaces are now outside any section."),
        "added" => Some("Added to the group."),
        "removed" => Some("Taken out of the group."),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The list of sections and spaces

/// A section with its spaces, for the list.
pub struct SectionRow {
    pub section: Option<Section>,
    pub spaces: Vec<SpaceTarget>,
}

#[derive(Template)]
#[template(path = "admin_spaces.html")]
struct SpacesTemplate {
    layout: Layout,
    notice: Option<&'static str>,
    errors: Vec<String>,
    rows: Vec<SectionRow>,
    any_spaces: bool,
}

impl SpacesTemplate {
    fn access(&self, space: &SpaceTarget) -> String {
        format::access_label(space.read_level(), &space.group_names)
            .unwrap_or_else(|| "Everyone".to_owned())
    }

    fn posting(&self, space: &SpaceTarget) -> &'static str {
        format::grant_label(space.rules.start.level)
    }
}

async fn render_spaces(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    notice: Option<&'static str>,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    let db = state.db();
    let spaces = content::readable_spaces(db, viewer).await?;
    let any_spaces = !spaces.is_empty();
    let mut rows = vec![SectionRow {
        section: None,
        spaces: Vec::new(),
    }];
    for section in content::sections(db).await? {
        rows.push(SectionRow {
            section: Some(section),
            spaces: Vec::new(),
        });
    }
    for space in spaces {
        let row = rows
            .iter_mut()
            .find(|row| row.section.as_ref().map(|s| s.id) == space.section_id)
            .expect("every space's section is listed");
        row.spaces.push(space);
    }
    if rows[0].spaces.is_empty() {
        rows.remove(0);
    }
    render(
        status,
        &SpacesTemplate {
            layout: Layout::load(state, viewer, Nav::Admin).await?,
            notice,
            errors,
            rows,
            any_spaces,
        },
    )
}

pub async fn spaces_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/spaces");
    render_spaces(
        &state,
        &viewer,
        StatusCode::OK,
        notice_for(&query.done),
        Vec::new(),
    )
    .await
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SectionForm {
    name: String,
    position: String,
}

impl SectionForm {
    fn parse(&self) -> Result<(String, i32), Vec<String>> {
        let mut problems = Vec::new();
        let name = store::clean_section_name(&self.name).map_err(|why| problems.push(why));
        let position = parse_position(&self.position).map_err(|why| problems.push(why));
        match (name, position) {
            (Ok(name), Ok(position)) => Ok((name, position)),
            _ => Err(problems),
        }
    }
}

fn parse_position(text: &str) -> Result<i32, String> {
    match text.trim() {
        "" => Ok(0),
        text => text
            .parse::<i32>()
            .map_err(|_| "The position should be a whole number, such as 3.".to_owned()),
    }
}

pub async fn create_section(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<SectionForm>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/spaces");
    match form.parse() {
        Err(problems) => {
            render_spaces(&state, &viewer, StatusCode::BAD_REQUEST, None, problems).await
        }
        Ok((name, position)) => {
            store::create_section(state.db(), &name, position).await?;
            state.changed(Change::SiteChanged).await?;
            Ok(see_other("/admin/spaces?done=section"))
        }
    }
}

pub async fn update_section(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<SectionForm>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/spaces");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match form.parse() {
        Err(problems) => {
            render_spaces(&state, &viewer, StatusCode::BAD_REQUEST, None, problems).await
        }
        Ok((name, position)) => {
            if !store::update_section(state.db(), id, &name, position).await? {
                return web::not_found(&state, &viewer).await;
            }
            state.changed(Change::SiteChanged).await?;
            Ok(see_other("/admin/spaces?done=section"))
        }
    }
}

#[derive(Template)]
#[template(path = "admin_delete_section.html")]
struct DeleteSectionTemplate {
    layout: Layout,
    section: Section,
    spaces: Vec<SpaceTarget>,
}

/// Asks before deleting a section, naming the spaces that will be
/// left outside any section.
pub async fn delete_section_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/spaces");
    let db = state.db();
    let section = match parse_id(&id) {
        Some(id) => content::sections(db)
            .await?
            .into_iter()
            .find(|section| section.id == id),
        None => None,
    };
    let Some(section) = section else {
        return web::not_found(&state, &viewer).await;
    };
    let spaces = content::readable_spaces(db, &viewer)
        .await?
        .into_iter()
        .filter(|space| space.section_id == Some(section.id))
        .collect();
    render(
        StatusCode::OK,
        &DeleteSectionTemplate {
            layout: Layout::load(&state, &viewer, Nav::Admin).await?,
            section,
            spaces,
        },
    )
}

pub async fn delete_section(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/spaces");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    if !store::delete_section(state.db(), id).await? {
        return web::not_found(&state, &viewer).await;
    }
    state.changed(Change::SiteChanged).await?;
    Ok(see_other("/admin/spaces?done=section-deleted"))
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
    let Some(space) = content::space_by_id(state.db(), &viewer, id).await? else {
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
pub struct MemberForm {
    username: String,
}

pub async fn add_member(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<MemberForm>,
) -> Result<Response, AppError> {
    let Some(user) = admin(&viewer) else {
        return not_admin(&state, &viewer, "/admin/groups").await;
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
            groups::add_member(state.db(), id, user_id, Some(added_by)).await?;
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
    let (Some(id), Some(user_id)) = (parse_id(&id), parse_id(&user_id)) else {
        return web::not_found(&state, &viewer).await;
    };
    if !groups::remove_member(state.db(), id, user_id).await? {
        return web::not_found(&state, &viewer).await;
    }
    Ok(see_other(&format!("/admin/groups/{id}?done=removed")))
}
