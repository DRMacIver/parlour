//! Admin pages for the shape of the forum (DESIGN.md §3.1), for admins only:
//!
//! - `/admin/categories`: the sections on the index and the categories in
//!   them, with who can read and post in each;
//! - `/admin/categories/new` and `/admin/categories/{id}`: a category's
//!   name, address, section, position, read and write levels and groups;
//! - `/admin/groups` and `/admin/groups/{id}`: groups and their members.
//!
//! Changing who can read a category clears every cached page that could
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
use crate::content::{self, CategoryTarget, Section};
use crate::error::AppError;
use crate::format;
use crate::groups::{self, GroupDetail, GroupFields, GroupSummary};
use crate::pages::parse_id;
use crate::store::{self, CategorySettings};
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
        "section-deleted" => Some("Section deleted. Its categories are now outside any section."),
        "added" => Some("Added to the group."),
        "removed" => Some("Taken out of the group."),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The list of sections and categories

/// A section with its categories, for the list.
pub struct SectionRow {
    pub section: Option<Section>,
    pub categories: Vec<CategoryTarget>,
}

#[derive(Template)]
#[template(path = "admin_categories.html")]
struct CategoriesTemplate {
    layout: Layout,
    notice: Option<&'static str>,
    errors: Vec<String>,
    rows: Vec<SectionRow>,
    any_categories: bool,
}

impl CategoriesTemplate {
    fn access(&self, category: &CategoryTarget) -> String {
        format::access_label(&category.read_level, &category.group_names)
            .unwrap_or_else(|| "Everyone".to_owned())
    }

    fn posting(&self, category: &CategoryTarget) -> &'static str {
        match category.write_level.as_str() {
            "members" => "Anyone who can read",
            "groups" => "Groups",
            _ => "Staff only",
        }
    }
}

async fn render_categories(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    notice: Option<&'static str>,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    let db = state.db();
    let categories = content::readable_categories(db, viewer).await?;
    let any_categories = !categories.is_empty();
    let mut rows = vec![SectionRow {
        section: None,
        categories: Vec::new(),
    }];
    for section in content::sections(db).await? {
        rows.push(SectionRow {
            section: Some(section),
            categories: Vec::new(),
        });
    }
    for category in categories {
        let row = rows
            .iter_mut()
            .find(|row| row.section.as_ref().map(|s| s.id) == category.section_id)
            .expect("every category's section is listed");
        row.categories.push(category);
    }
    if rows[0].categories.is_empty() {
        rows.remove(0);
    }
    render(
        status,
        &CategoriesTemplate {
            layout: Layout::load(state, viewer, Nav::Admin).await?,
            notice,
            errors,
            rows,
            any_categories,
        },
    )
}

pub async fn categories_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/categories");
    render_categories(
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
    admins_only!(state, viewer, "/admin/categories");
    match form.parse() {
        Err(problems) => {
            render_categories(&state, &viewer, StatusCode::BAD_REQUEST, None, problems).await
        }
        Ok((name, position)) => {
            store::create_section(state.db(), &name, position).await?;
            state.changed(Change::SiteChanged).await?;
            Ok(see_other("/admin/categories?done=section"))
        }
    }
}

pub async fn update_section(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<SectionForm>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/categories");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match form.parse() {
        Err(problems) => {
            render_categories(&state, &viewer, StatusCode::BAD_REQUEST, None, problems).await
        }
        Ok((name, position)) => {
            if !store::update_section(state.db(), id, &name, position).await? {
                return web::not_found(&state, &viewer).await;
            }
            state.changed(Change::SiteChanged).await?;
            Ok(see_other("/admin/categories?done=section"))
        }
    }
}

pub async fn delete_section(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/categories");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    if !store::delete_section(state.db(), id).await? {
        return web::not_found(&state, &viewer).await;
    }
    state.changed(Change::SiteChanged).await?;
    Ok(see_other("/admin/categories?done=section-deleted"))
}

// ---------------------------------------------------------------------------
// One category

/// The category form, as typed.
#[derive(Clone, Debug, Default)]
pub struct CategoryForm {
    name: String,
    slug: String,
    description: String,
    section: String,
    position: String,
    read_level: String,
    write_level: String,
    read_groups: Vec<String>,
    write_groups: Vec<String>,
}

impl CategoryForm {
    fn from_fields(fields: &[(String, String)]) -> CategoryForm {
        let mut form = CategoryForm::default();
        for (key, value) in fields {
            match key.as_str() {
                "name" => form.name = value.clone(),
                "slug" => form.slug = value.clone(),
                "description" => form.description = value.clone(),
                "section" => form.section = value.clone(),
                "position" => form.position = value.clone(),
                "read_level" => form.read_level = value.clone(),
                "write_level" => form.write_level = value.clone(),
                "read_group" => form.read_groups.push(value.clone()),
                "write_group" => form.write_groups.push(value.clone()),
                _ => {}
            }
        }
        form
    }

    fn from_settings(settings: &CategorySettings) -> CategoryForm {
        let ids = |ids: &[i64]| ids.iter().map(i64::to_string).collect();
        CategoryForm {
            name: settings.name.clone(),
            slug: settings.slug.clone(),
            description: settings.description.clone(),
            section: settings
                .section_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            position: settings.position.to_string(),
            read_level: settings.read_level.clone(),
            write_level: settings.write_level.clone(),
            read_groups: ids(&settings.read_groups),
            write_groups: ids(&settings.write_groups),
        }
    }

    /// The settings this form describes, checked against the sections and
    /// groups there are.
    fn settings(
        &self,
        sections: &[Section],
        groups: &[GroupSummary],
    ) -> Result<CategorySettings, Vec<String>> {
        let mut problems = Vec::new();
        let section_id = match self.section.trim() {
            "" => None,
            id => match id.parse::<i64>() {
                Ok(id) if sections.iter().any(|s| s.id == id) => Some(id),
                _ => {
                    problems.push("Choose a section from the list.".to_owned());
                    None
                }
            },
        };
        let position = parse_position(&self.position).unwrap_or_else(|why| {
            problems.push(why);
            0
        });
        let mut group_ids = |chosen: &[String]| -> Vec<i64> {
            let ids: Vec<i64> = chosen
                .iter()
                .filter_map(|id| id.trim().parse::<i64>().ok())
                .filter(|id| groups.iter().any(|g| g.id == *id))
                .collect();
            if ids.len() != chosen.len() {
                problems.push("Choose groups from the list.".to_owned());
            }
            ids
        };
        let read_groups = group_ids(&self.read_groups);
        let write_groups = group_ids(&self.write_groups);
        problems.dedup();
        let settings = CategorySettings {
            section_id,
            slug: self.slug.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            position,
            read_level: self.read_level.clone(),
            write_level: self.write_level.clone(),
            read_groups,
            write_groups,
        }
        .clean();
        match settings {
            Ok(settings) if problems.is_empty() => Ok(settings),
            Ok(_) => Err(problems),
            Err(more) => {
                problems.extend(more);
                Err(problems)
            }
        }
    }
}

#[derive(Template)]
#[template(path = "admin_category.html")]
struct CategoryTemplate {
    layout: Layout,
    /// `None` for a new category.
    id: Option<i64>,
    errors: Vec<String>,
    form: CategoryForm,
    sections: Vec<Section>,
    groups: Vec<GroupSummary>,
    thread_count: Option<i64>,
}

impl CategoryTemplate {
    fn action(&self) -> String {
        match self.id {
            Some(id) => format!("/admin/categories/{id}"),
            None => "/admin/categories/new".to_owned(),
        }
    }

    fn section_chosen(&self, id: &i64) -> bool {
        self.form.section.trim() == id.to_string()
    }

    fn reads(&self, id: &i64) -> bool {
        self.form.read_groups.contains(&id.to_string())
    }

    fn writes(&self, id: &i64) -> bool {
        self.form.write_groups.contains(&id.to_string())
    }
}

async fn render_category(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    id: Option<i64>,
    form: CategoryForm,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    let db = state.db();
    let thread_count = match id {
        Some(id) => Some(content::category_thread_count(db, viewer, id).await?),
        None => None,
    };
    render(
        status,
        &CategoryTemplate {
            layout: Layout::load(state, viewer, Nav::Admin).await?,
            id,
            errors,
            form,
            sections: content::sections(db).await?,
            groups: groups::list(db).await?,
            thread_count,
        },
    )
}

pub async fn new_category_page(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/categories/new");
    let form = CategoryForm {
        read_level: "public".to_owned(),
        write_level: "members".to_owned(),
        position: "0".to_owned(),
        ..CategoryForm::default()
    };
    render_category(&state, &viewer, StatusCode::OK, None, form, Vec::new()).await
}

pub async fn create_category(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/categories/new");
    let form = CategoryForm::from_fields(&fields);
    let db = state.db();
    let refuse = |problems| {
        render_category(
            &state,
            &viewer,
            StatusCode::BAD_REQUEST,
            None,
            form.clone(),
            problems,
        )
    };
    let settings = match form.settings(&content::sections(db).await?, &groups::list(db).await?) {
        Ok(settings) => settings,
        Err(problems) => return refuse(problems).await,
    };
    match store::add_category(db, &settings).await? {
        Err(why) => refuse(vec![why]).await,
        Ok(category_id) => {
            state
                .changed(Change::CategoryChanged { category_id })
                .await?;
            Ok(see_other("/admin/categories?done=saved"))
        }
    }
}

pub async fn category_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/categories");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(category) = content::category_by_id(state.db(), &viewer, id).await? else {
        return web::not_found(&state, &viewer).await;
    };
    let form = CategoryForm::from_settings(&category.settings());
    render_category(&state, &viewer, StatusCode::OK, Some(id), form, Vec::new()).await
}

pub async fn update_category(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/categories");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let db = state.db();
    if content::category_by_id(db, &viewer, id).await?.is_none() {
        return web::not_found(&state, &viewer).await;
    }
    let form = CategoryForm::from_fields(&fields);
    let refuse = |problems| {
        render_category(
            &state,
            &viewer,
            StatusCode::BAD_REQUEST,
            Some(id),
            form.clone(),
            problems,
        )
    };
    let settings = match form.settings(&content::sections(db).await?, &groups::list(db).await?) {
        Ok(settings) => settings,
        Err(problems) => return refuse(problems).await,
    };
    match store::update_category(db, id, &settings).await? {
        Err(why) => refuse(vec![why]).await,
        Ok(false) => web::not_found(&state, &viewer).await,
        Ok(true) => {
            // A new read level changes every page that could show the
            // category's threads; this clears them all.
            state
                .changed(Change::CategoryChanged { category_id: id })
                .await?;
            Ok(see_other("/admin/categories?done=saved"))
        }
    }
}

pub async fn delete_category(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/categories");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match store::delete_category(state.db(), id).await? {
        Ok(true) => {
            state
                .changed(Change::CategoryChanged { category_id: id })
                .await?;
            Ok(see_other("/admin/categories?done=deleted"))
        }
        Ok(false) => web::not_found(&state, &viewer).await,
        Err(why) => {
            let Some(category) = content::category_by_id(state.db(), &viewer, id).await? else {
                return web::not_found(&state, &viewer).await;
            };
            render_category(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                Some(id),
                CategoryForm::from_settings(&category.settings()),
                vec![why],
            )
            .await
        }
    }
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

pub async fn delete_group(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    admins_only!(state, viewer, "/admin/groups");
    let deleted = match parse_id(&id) {
        Some(id) => groups::delete(state.db(), id).await?,
        None => false,
    };
    if !deleted {
        return web::not_found(&state, &viewer).await;
    }
    Ok(see_other("/admin/groups?done=deleted"))
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
