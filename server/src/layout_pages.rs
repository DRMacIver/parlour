//! The layout editors (DESIGN.md §3.5, §5): a member arranging their own
//! front page at `/settings/layout`, and admins arranging the default one
//! at `/admin/layout`. Both are the same editor over a different layout.
//!
//! Each works as plain forms: add a section, rename it, move it up or
//! down, delete it (asking first), add a space to a section, move a space
//! up or down within its section, and take it off the front page. The
//! layout editor island (`web/src/islands/LayoutEditor.tsx`) replaces
//! those forms with one that drags spaces between sections, and saves
//! each change through the JSON endpoints here, under `/api/layout` and
//! `/api/admin/layout`, which answer with the layout as it now is.
//!
//! The editor shows only the spaces the viewer can read. The others stay
//! where they are: a space moved up goes before the previous space the
//! viewer can see, and one dropped before a space goes just before it.
//!
//! The member's page also lists the spaces they have muted, with Unmute
//! forms that come back here. The admin's page also marks spaces
//! essential, and applies the default again to every member who hasn't
//! arranged their own front page ("apply to members who haven't edited
//! theirs"); each change to the default reaches them anyway, as it is
//! made.
//!
//! Any change to a member's layout makes it theirs (`edited_at`), so the
//! default stops reaching it. Changes to the default clear every cached
//! page that shows it; a member's own layout is only ever shown to them,
//! on pages that are never cached.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::admin_pages::DoneQuery;
use crate::api;
use crate::changes::Change;
use crate::content::{self, ArrangedSection, ArrangedSpace, Arrangement};
use crate::error::AppError;
use crate::layouts;
use crate::pages::parse_id;
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, render, see_other};

/// Which layout an editor is for: the viewer's own, or the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Mine,
    Default,
}

impl Which {
    const fn of(admin: bool) -> Which {
        if admin { Which::Default } else { Which::Mine }
    }

    /// Where the editor's page and plain forms are.
    pub fn prefix(self) -> &'static str {
        match self {
            Which::Mine => "/settings/layout",
            Which::Default => "/admin/layout",
        }
    }

    /// Where the island's endpoints are.
    pub fn api(self) -> &'static str {
        match self {
            Which::Mine => "/api/layout",
            Which::Default => "/api/admin/layout",
        }
    }
}

/// Someone allowed to edit a layout, and which one.
struct Editor {
    which: Which,
    layout_id: i64,
}

/// Why an editor may not have the page or make the change.
enum Barred {
    LogIn,
    Forbidden,
}

/// The editor for this viewer, if they may use it. Anyone logged in may
/// look at their own; a suspended member can't change it. The default's
/// editor is for admins (a suspended admin isn't one for now).
async fn editor(
    state: &AppState,
    viewer: &Viewer,
    which: Which,
    writing: bool,
) -> Result<Result<Editor, Barred>, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(Err(Barred::LogIn));
    };
    let layout_id = match which {
        Which::Mine => {
            if writing && !user.may_act() {
                return Ok(Err(Barred::Forbidden));
            }
            layouts::own_id(state.db(), user.id).await?
        }
        Which::Default => {
            if !viewer.is_admin() {
                return Ok(Err(Barred::Forbidden));
            }
            layouts::default_id(state.db()).await?
        }
    };
    Ok(Ok(Editor { which, layout_id }))
}

/// [`editor`] for the pages and plain forms.
async fn page_editor(
    state: &AppState,
    viewer: &Viewer,
    which: Which,
    writing: bool,
) -> Result<Result<Editor, Response>, AppError> {
    Ok(match editor(state, viewer, which, writing).await? {
        Ok(editor) => Ok(editor),
        Err(Barred::LogIn) => Err(web::login_first(which.prefix())),
        Err(Barred::Forbidden) => Err(web::forbidden(state, viewer).await?),
    })
}

/// [`editor`] for the island's endpoints.
async fn api_editor(
    state: &AppState,
    viewer: &Viewer,
    which: Which,
) -> Result<Result<Editor, Response>, AppError> {
    Ok(match editor(state, viewer, which, true).await? {
        Ok(editor) => Ok(editor),
        Err(Barred::LogIn) => Err(api::log_in()),
        Err(Barred::Forbidden) => Err(api::error(
            StatusCode::FORBIDDEN,
            "You don’t have permission to do that.",
        )),
    })
}

// ---------------------------------------------------------------------------
// The changes, shared by the plain forms and the island

/// Why a change wasn't made.
enum Failure {
    /// No such section in this layout, or no such space the viewer can
    /// read.
    NotFound,
    /// What was asked for doesn't make sense, and this says why.
    Invalid(String),
}

type Edit = Result<(), Failure>;

impl Editor {
    async fn arrangement(
        &self,
        state: &AppState,
        viewer: &Viewer,
    ) -> Result<Arrangement, AppError> {
        Ok(content::arrangement(state.db(), viewer, self.layout_id)
            .await?
            .expect("the editor's layout exists"))
    }

    /// The raw order of a section's spaces, including any the viewer
    /// can't see.
    async fn stored_section(
        &self,
        state: &AppState,
        section_id: i64,
    ) -> Result<Option<Vec<i64>>, AppError> {
        Ok(layouts::read(state.db(), self.layout_id)
            .await?
            .and_then(|layout| layout.sections.into_iter().find(|s| s.id == section_id))
            .map(|section| section.spaces.iter().map(|c| c.id).collect()))
    }

    /// What every change ends with: changes to the default clear the
    /// pages that show it.
    async fn changed(&self, state: &AppState) -> Result<(), AppError> {
        if self.which == Which::Default {
            state.changed(Change::SiteChanged).await?;
        }
        Ok(())
    }

    async fn add_section(&self, state: &AppState, name: &str) -> Result<Edit, AppError> {
        let name = match layouts::clean_section_name(name) {
            Ok(name) => name,
            Err(why) => return Ok(Err(Failure::Invalid(why))),
        };
        layouts::add_section(state.db(), self.layout_id, &name).await?;
        self.changed(state).await?;
        Ok(Ok(()))
    }

    async fn rename_section(
        &self,
        state: &AppState,
        section_id: i64,
        name: &str,
    ) -> Result<Edit, AppError> {
        let name = match layouts::clean_section_name(name) {
            Ok(name) => name,
            Err(why) => return Ok(Err(Failure::Invalid(why))),
        };
        if !layouts::rename_section(state.db(), self.layout_id, section_id, &name).await? {
            return Ok(Err(Failure::NotFound));
        }
        self.changed(state).await?;
        Ok(Ok(()))
    }

    async fn delete_section(&self, state: &AppState, section_id: i64) -> Result<Edit, AppError> {
        if !layouts::delete_section(state.db(), self.layout_id, section_id).await? {
            return Ok(Err(Failure::NotFound));
        }
        self.changed(state).await?;
        Ok(Ok(()))
    }

    /// Puts the sections in this order (any left out keep their order
    /// after them).
    async fn order_sections(&self, state: &AppState, order: &[i64]) -> Result<Edit, AppError> {
        if !layouts::reorder_sections(state.db(), self.layout_id, order).await? {
            return Ok(Err(Failure::NotFound));
        }
        self.changed(state).await?;
        Ok(Ok(()))
    }

    /// Swaps a section with the one before it (`up`) or after it. At the
    /// top or bottom already, nothing changes.
    async fn step_section(
        &self,
        state: &AppState,
        section_id: i64,
        up: bool,
    ) -> Result<Edit, AppError> {
        let Some(layout) = layouts::read(state.db(), self.layout_id).await? else {
            return Ok(Err(Failure::NotFound));
        };
        let mut order: Vec<i64> = layout.sections.iter().map(|s| s.id).collect();
        let Some(at) = order.iter().position(|&id| id == section_id) else {
            return Ok(Err(Failure::NotFound));
        };
        let other = if up { at.checked_sub(1) } else { Some(at + 1) };
        match other {
            Some(other) if other < order.len() => {
                order.swap(at, other);
                self.order_sections(state, &order).await
            }
            _ => Ok(Ok(())),
        }
    }

    /// Puts a space the viewer can read in a section: before the space
    /// the viewer sees at `index` in it (counting without the space being
    /// moved), or at the end.
    async fn place_space(
        &self,
        state: &AppState,
        viewer: &Viewer,
        space_id: i64,
        section_id: i64,
        index: Option<usize>,
    ) -> Result<Edit, AppError> {
        if content::space_by_id(state.db(), viewer, space_id)
            .await?
            .is_none()
        {
            return Ok(Err(Failure::NotFound));
        }
        let arrangement = self.arrangement(state, viewer).await?;
        let Some(section) = arrangement.section(section_id) else {
            return Ok(Err(Failure::NotFound));
        };
        let Some(stored) = self.stored_section(state, section_id).await? else {
            return Ok(Err(Failure::NotFound));
        };
        let others: Vec<i64> = stored.into_iter().filter(|&id| id != space_id).collect();
        let seen: Vec<i64> = section
            .spaces
            .iter()
            .map(|c| c.id)
            .filter(|&id| id != space_id)
            .collect();
        let target = match index {
            Some(index) if index < seen.len() => others
                .iter()
                .position(|&id| id == seen[index])
                .unwrap_or(others.len()),
            _ => others.len(),
        };
        let target = i32::try_from(target).unwrap_or(i32::MAX);
        if !layouts::move_space(state.db(), self.layout_id, space_id, section_id, target).await? {
            return Ok(Err(Failure::NotFound));
        }
        self.changed(state).await?;
        Ok(Ok(()))
    }

    /// Moves a space before the previous space the viewer can see in its
    /// section (`up`; the top if there is none), or after the next one
    /// (the end if there is none).
    async fn step_space(
        &self,
        state: &AppState,
        viewer: &Viewer,
        space_id: i64,
        up: bool,
    ) -> Result<Edit, AppError> {
        let arrangement = self.arrangement(state, viewer).await?;
        let Some(section) = arrangement.section_of(space_id) else {
            return Ok(Err(Failure::NotFound));
        };
        let Some(stored) = self.stored_section(state, section.id).await? else {
            return Ok(Err(Failure::NotFound));
        };
        let seen = |id: i64| section.spaces.iter().any(|c| c.id == id);
        let Some(at) = stored.iter().position(|&id| id == space_id) else {
            return Ok(Err(Failure::NotFound));
        };
        let others: Vec<i64> = stored
            .iter()
            .copied()
            .filter(|&id| id != space_id)
            .collect();
        let target = if up {
            stored[..at].iter().rposition(|&id| seen(id)).unwrap_or(0)
        } else {
            match stored[at + 1..].iter().position(|&id| seen(id)) {
                // In `others` the next one is at `at + n`; just after it.
                Some(n) => at + n + 1,
                None => others.len(),
            }
        };
        if target == at {
            return Ok(Ok(()));
        }
        let target = i32::try_from(target).unwrap_or(i32::MAX);
        if !layouts::move_space(state.db(), self.layout_id, space_id, section.id, target).await? {
            return Ok(Err(Failure::NotFound));
        }
        self.changed(state).await?;
        Ok(Ok(()))
    }

    /// Takes a space the viewer can read off the front page.
    async fn remove_space(
        &self,
        state: &AppState,
        viewer: &Viewer,
        space_id: i64,
    ) -> Result<Edit, AppError> {
        if content::space_by_id(state.db(), viewer, space_id)
            .await?
            .is_none()
        {
            return Ok(Err(Failure::NotFound));
        }
        if layouts::unfollow(state.db(), self.layout_id, space_id).await? {
            self.changed(state).await?;
        }
        Ok(Ok(()))
    }

    /// Marks a space essential, or not (the default's editor only). This
    /// is an admin's power over every front page, so it is always logged.
    async fn set_essential(
        &self,
        state: &AppState,
        viewer: &Viewer,
        space_id: i64,
        essential: bool,
    ) -> Result<Edit, AppError> {
        let Some(space) = content::space_by_id(state.db(), viewer, space_id).await? else {
            return Ok(Err(Failure::NotFound));
        };
        if self.which != Which::Default {
            return Ok(Err(Failure::NotFound));
        }
        if layouts::set_essential(state.db(), space_id, essential).await? {
            let action = if essential {
                "mark_space_essential"
            } else {
                "unmark_space_essential"
            };
            if let Some(user) = viewer.user() {
                crate::space_pages::record_space_action(state, user.id, &space, action).await?;
            }
            state.changed(Change::SpaceChanged { space_id }).await?;
        }
        Ok(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// The page

/// One choice in a section's "add a space" select.
pub struct AddOption {
    pub id: i64,
    pub label: String,
}

#[derive(Template)]
#[template(path = "layout_editor.html")]
struct EditorTemplate {
    layout: Layout,
    admin: bool,
    prefix: &'static str,
    notice: Option<&'static str>,
    errors: Vec<String>,
    arrangement: Arrangement,
    /// For the default: how many members follow it.
    followers: i64,
    /// The island's props, when the page has islands.
    props: Option<String>,
}

impl EditorTemplate {
    /// What a section's "add a space" select offers: the spaces on no
    /// section, then those in other sections, saying where they are.
    fn options(&self, section: &ArrangedSection) -> Vec<AddOption> {
        let mut options: Vec<AddOption> = self
            .arrangement
            .available
            .iter()
            .map(|c| AddOption {
                id: c.id,
                label: c.name.clone(),
            })
            .collect();
        for other in &self.arrangement.sections {
            if other.id == section.id {
                continue;
            }
            for c in other.spaces.iter().filter(|c| !c.archived) {
                options.push(AddOption {
                    id: c.id,
                    label: format!("{} (in {})", c.name, other.name),
                });
            }
        }
        options
    }

    fn is_first(&self, section: &ArrangedSection) -> bool {
        self.arrangement
            .sections
            .first()
            .is_some_and(|s| s.id == section.id)
    }

    fn is_last(&self, section: &ArrangedSection) -> bool {
        self.arrangement
            .sections
            .last()
            .is_some_and(|s| s.id == section.id)
    }

    fn unmute_next(&self) -> String {
        format!("{}?done=unmuted", self.prefix)
    }
}

fn notice_for(which: Which, done: &str) -> Option<&'static str> {
    match (which, done) {
        (_, "saved") => Some("Saved."),
        (_, "section") => Some("Section saved."),
        (Which::Mine, "section-deleted") => {
            Some("Section deleted. Its spaces are off your front page now.")
        }
        (Which::Default, "section-deleted") => {
            Some("Section deleted. Its spaces are off the default front page now.")
        }
        (Which::Mine, "unmuted") => Some("Unmuted."),
        (Which::Default, "applied") => Some(
            "Applied: every member who hasn’t arranged their own front page now has the default as it is.",
        ),
        _ => None,
    }
}

async fn render_editor(
    state: &AppState,
    viewer: &Viewer,
    editor: &Editor,
    status: StatusCode,
    notice: Option<&'static str>,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    let arrangement = editor.arrangement(state, viewer).await?;
    let admin = editor.which == Which::Default;
    let followers = if admin {
        layouts::follower_count(state.db()).await?
    } else {
        0
    };
    let nav = if admin { Nav::Admin } else { Nav::Account };
    let layout = Layout::load(state, viewer, nav).await?;
    let props = layout
        .islands
        .is_some()
        .then(|| crate::islands::layout_editor_props(editor.which.api(), admin, &arrangement));
    render(
        status,
        &EditorTemplate {
            layout,
            admin,
            prefix: editor.which.prefix(),
            notice,
            errors,
            arrangement,
            followers,
            props,
        },
    )
}

/// `GET /settings/layout` and `GET /admin/layout`.
pub async fn editor_page<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    let which = Which::of(ADMIN);
    let editor = match page_editor(&state, &viewer, which, false).await? {
        Ok(editor) => editor,
        Err(response) => return Ok(response),
    };
    render_editor(
        &state,
        &viewer,
        &editor,
        StatusCode::OK,
        notice_for(which, &query.done),
        Vec::new(),
    )
    .await
}

/// What a plain form's change comes to: back to the editor with a
/// notice, or the page saying what went wrong.
async fn answer(
    state: &AppState,
    viewer: &Viewer,
    editor: &Editor,
    edit: Edit,
    done: &str,
) -> Result<Response, AppError> {
    match edit {
        Ok(()) => Ok(see_other(&format!("{}?done={done}", editor.which.prefix()))),
        Err(Failure::NotFound) => web::not_found(state, viewer).await,
        Err(Failure::Invalid(why)) => {
            render_editor(
                state,
                viewer,
                editor,
                StatusCode::BAD_REQUEST,
                None,
                vec![why],
            )
            .await
        }
    }
}

/// Looks up the editor for a plain form, or returns the response to send
/// instead.
macro_rules! form_editor {
    ($state:expr, $viewer:expr, $admin:expr) => {
        match page_editor(&$state, &$viewer, Which::of($admin), true).await? {
            Ok(editor) => editor,
            Err(response) => return Ok(response),
        }
    };
}

/// An id from a path, or the not-found page.
macro_rules! path_id {
    ($state:expr, $viewer:expr, $id:expr) => {
        match parse_id(&$id) {
            Some(id) => id,
            None => return web::not_found(&$state, &$viewer).await,
        }
    };
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NameForm {
    name: String,
}

pub async fn add_section<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<NameForm>,
) -> Result<Response, AppError> {
    let editor = form_editor!(state, viewer, ADMIN);
    let edit = editor.add_section(&state, &form.name).await?;
    answer(&state, &viewer, &editor, edit, "section").await
}

pub async fn rename_section<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<NameForm>,
) -> Result<Response, AppError> {
    let editor = form_editor!(state, viewer, ADMIN);
    let id = path_id!(state, viewer, id);
    let edit = editor.rename_section(&state, id, &form.name).await?;
    answer(&state, &viewer, &editor, edit, "section").await
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DirectionForm {
    direction: String,
}

impl DirectionForm {
    /// True for up, false for down.
    fn up(&self) -> Result<bool, Failure> {
        match self.direction.as_str() {
            "up" => Ok(true),
            "down" => Ok(false),
            _ => Err(Failure::Invalid("Choose to move it up or down.".to_owned())),
        }
    }
}

pub async fn move_section<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<DirectionForm>,
) -> Result<Response, AppError> {
    let editor = form_editor!(state, viewer, ADMIN);
    let id = path_id!(state, viewer, id);
    let edit = match form.up() {
        Ok(up) => editor.step_section(&state, id, up).await?,
        Err(failure) => Err(failure),
    };
    answer(&state, &viewer, &editor, edit, "saved").await
}

#[derive(Template)]
#[template(path = "layout_delete_section.html")]
struct DeleteSectionTemplate {
    layout: Layout,
    admin: bool,
    prefix: &'static str,
    section: ArrangedSection,
}

/// Asks before deleting a section, naming the spaces that will leave the
/// front page.
pub async fn delete_section_page<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let editor = match page_editor(&state, &viewer, Which::of(ADMIN), false).await? {
        Ok(editor) => editor,
        Err(response) => return Ok(response),
    };
    let id = path_id!(state, viewer, id);
    let Some(section) = editor
        .arrangement(&state, &viewer)
        .await?
        .sections
        .into_iter()
        .find(|s| s.id == id)
    else {
        return web::not_found(&state, &viewer).await;
    };
    let nav = if ADMIN { Nav::Admin } else { Nav::Account };
    render(
        StatusCode::OK,
        &DeleteSectionTemplate {
            layout: Layout::load(&state, &viewer, nav).await?,
            admin: ADMIN,
            prefix: editor.which.prefix(),
            section,
        },
    )
}

pub async fn delete_section<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let editor = form_editor!(state, viewer, ADMIN);
    let id = path_id!(state, viewer, id);
    let edit = editor.delete_section(&state, id).await?;
    answer(&state, &viewer, &editor, edit, "section-deleted").await
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SpaceForm {
    space: String,
}

/// Adds a space to the end of a section, from wherever it was.
pub async fn add_space<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<SpaceForm>,
) -> Result<Response, AppError> {
    let editor = form_editor!(state, viewer, ADMIN);
    let section_id = path_id!(state, viewer, id);
    let space_id = path_id!(state, viewer, form.space);
    let edit = editor
        .place_space(&state, &viewer, space_id, section_id, None)
        .await?;
    answer(&state, &viewer, &editor, edit, "saved").await
}

pub async fn move_space<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<DirectionForm>,
) -> Result<Response, AppError> {
    let editor = form_editor!(state, viewer, ADMIN);
    let id = path_id!(state, viewer, id);
    let edit = match form.up() {
        Ok(up) => editor.step_space(&state, &viewer, id, up).await?,
        Err(failure) => Err(failure),
    };
    answer(&state, &viewer, &editor, edit, "saved").await
}

pub async fn remove_space<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let editor = form_editor!(state, viewer, ADMIN);
    let id = path_id!(state, viewer, id);
    let edit = editor.remove_space(&state, &viewer, id).await?;
    answer(&state, &viewer, &editor, edit, "saved").await
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct EssentialForm {
    essential: String,
}

/// `POST /admin/layout/spaces/{id}/essential`.
pub async fn essential(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(form): Form<EssentialForm>,
) -> Result<Response, AppError> {
    let editor = form_editor!(state, viewer, true);
    let id = path_id!(state, viewer, id);
    let edit = match form.essential.as_str() {
        "1" | "on" | "true" => editor.set_essential(&state, &viewer, id, true).await?,
        "0" | "off" | "false" | "" => editor.set_essential(&state, &viewer, id, false).await?,
        _ => Err(Failure::Invalid(
            "Say whether the space is to be essential.".to_owned(),
        )),
    };
    answer(&state, &viewer, &editor, edit, "saved").await
}

/// `POST /admin/layout/apply`: copies the default again into the layout
/// of every member who hasn't arranged their own.
pub async fn apply(State(state): State<AppState>, viewer: Viewer) -> Result<Response, AppError> {
    let editor = form_editor!(state, viewer, true);
    layouts::default_changed(state.db()).await?;
    answer(&state, &viewer, &editor, Ok(()), "applied").await
}

// ---------------------------------------------------------------------------
// The island's endpoints

/// What an endpoint answers: the layout as it now is, or why not.
async fn api_answer(
    state: &AppState,
    viewer: &Viewer,
    editor: &Editor,
    edit: Edit,
) -> Result<Response, AppError> {
    match edit {
        Ok(()) => {
            let arrangement = editor.arrangement(state, viewer).await?;
            Ok(Json(json!({ "layout": arrangement })).into_response())
        }
        Err(Failure::NotFound) => Ok(api::not_found()),
        Err(Failure::Invalid(why)) => Ok(api::error(StatusCode::BAD_REQUEST, &why)),
    }
}

macro_rules! api_editor {
    ($state:expr, $viewer:expr, $admin:expr) => {
        match api_editor(&$state, &$viewer, Which::of($admin)).await? {
            Ok(editor) => editor,
            Err(response) => return Ok(response),
        }
    };
}

macro_rules! api_id {
    ($id:expr) => {
        match parse_id(&$id) {
            Some(id) => id,
            None => return Ok(api::not_found()),
        }
    };
}

#[derive(Deserialize)]
pub struct NameRequest {
    name: String,
}

pub async fn api_add_section<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Json(request): Json<NameRequest>,
) -> Result<Response, AppError> {
    let editor = api_editor!(state, viewer, ADMIN);
    let edit = editor.add_section(&state, &request.name).await?;
    api_answer(&state, &viewer, &editor, edit).await
}

pub async fn api_rename_section<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Json(request): Json<NameRequest>,
) -> Result<Response, AppError> {
    let editor = api_editor!(state, viewer, ADMIN);
    let id = api_id!(id);
    let edit = editor.rename_section(&state, id, &request.name).await?;
    api_answer(&state, &viewer, &editor, edit).await
}

pub async fn api_delete_section<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let editor = api_editor!(state, viewer, ADMIN);
    let id = api_id!(id);
    let edit = editor.delete_section(&state, id).await?;
    api_answer(&state, &viewer, &editor, edit).await
}

#[derive(Deserialize)]
pub struct OrderRequest {
    sections: Vec<i64>,
}

pub async fn api_order_sections<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Json(request): Json<OrderRequest>,
) -> Result<Response, AppError> {
    let editor = api_editor!(state, viewer, ADMIN);
    let edit = editor.order_sections(&state, &request.sections).await?;
    api_answer(&state, &viewer, &editor, edit).await
}

#[derive(Deserialize)]
pub struct MoveRequest {
    section: i64,
    /// Before the space the viewer sees at this index in the section,
    /// counting without the one being moved; past the end (or none) is
    /// the end.
    #[serde(default)]
    index: Option<usize>,
}

pub async fn api_move_space<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Json(request): Json<MoveRequest>,
) -> Result<Response, AppError> {
    let editor = api_editor!(state, viewer, ADMIN);
    let id = api_id!(id);
    let edit = editor
        .place_space(&state, &viewer, id, request.section, request.index)
        .await?;
    api_answer(&state, &viewer, &editor, edit).await
}

pub async fn api_remove_space<const ADMIN: bool>(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let editor = api_editor!(state, viewer, ADMIN);
    let id = api_id!(id);
    let edit = editor.remove_space(&state, &viewer, id).await?;
    api_answer(&state, &viewer, &editor, edit).await
}

#[derive(Deserialize)]
pub struct EssentialRequest {
    essential: bool,
}

pub async fn api_essential(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Json(request): Json<EssentialRequest>,
) -> Result<Response, AppError> {
    let editor = api_editor!(state, viewer, true);
    let id = api_id!(id);
    let edit = editor
        .set_essential(&state, &viewer, id, request.essential)
        .await?;
    api_answer(&state, &viewer, &editor, edit).await
}

/// For the template: a space's flags as words.
pub fn flags(space: &ArrangedSpace) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if space.essential {
        flags.push("Essential");
    }
    if space.archived {
        flags.push("Archived");
    }
    flags
}
