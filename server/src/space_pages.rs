//! Spaces as members make and run them (DESIGN.md §3.1): the space index
//! at `/spaces`, making one at `/spaces/new`, and a space's settings,
//! archiving and deletion at `/c/{slug}/settings` and its neighbours.
//!
//! Anyone who administers a space (its administer grant names them, or
//! they are staff) may change it. Staff changing a space they don't
//! administer in their own right is recorded in the moderation log.

use std::collections::HashMap;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::admin_pages::DoneQuery;
use crate::changes::Change;
use crate::content::{self, Deps, Section, SpaceListing, SpaceSort, SpaceTarget};
use crate::error::AppError;
use crate::groups::{self, GroupSummary};
use crate::mod_records::{self, NewAction};
use crate::permissions::{Grant, Level};
use crate::ratelimit::{Action, Scope};
use crate::settings::{self, SpaceCreation};
use crate::store::{self, SpaceSettings};
use crate::users;
use crate::viewer::{UserCtx, Viewer};
use crate::web::{self, Layout, Nav, cacheable, redirect_unless_canonical, render, see_other};

/// Whether the viewer may make a space: a member in good standing, and
/// either staff or anyone when the site allows it.
async fn may_create(state: &AppState, viewer: &Viewer) -> Result<bool, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(false);
    };
    if !user.may_write() {
        return Ok(false);
    }
    Ok(user.is_staff() || settings::space_creation(state.db()).await? == SpaceCreation::Members)
}

// ---------------------------------------------------------------------------
// The space index

/// The one address for each view of the index, so that other spellings
/// redirect there (DESIGN.md §5).
fn index_href(sort: SpaceSort, archived: bool) -> String {
    let mut href = String::from("/spaces");
    let mut sep = '?';
    if sort != SpaceSort::Created {
        href.push(sep);
        href.push_str("sort=");
        href.push_str(sort.as_str());
        sep = '&';
    }
    if archived {
        href.push(sep);
        href.push_str("archived=1");
    }
    href
}

#[derive(Template)]
#[template(path = "spaces.html")]
struct SpacesTemplate {
    layout: Layout,
    spaces: Vec<SpaceListing>,
    sort: SpaceSort,
    archived: bool,
    can_create: bool,
}

impl SpacesTemplate {
    fn sort_is(&self, sort: &str) -> bool {
        self.sort.as_str() == sort
    }

    fn sort_href(&self, sort: &str) -> String {
        index_href(
            SpaceSort::parse(sort).unwrap_or(SpaceSort::Created),
            self.archived,
        )
    }

    fn archived_href(&self) -> String {
        index_href(self.sort, !self.archived)
    }
}

pub async fn index(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    let Query(params) = Query::<Vec<(String, String)>>::try_from_uri(&uri).unwrap_or_default();
    let mut sorts = params.iter().filter(|(key, _)| key == "sort");
    let sort = match (sorts.next(), sorts.next()) {
        (None, _) => Some(SpaceSort::Created),
        (Some((_, value)), None) => SpaceSort::parse(value),
        (Some(_), Some(_)) => None,
    };
    let Some(sort) = sort else {
        return web::not_found(&state, &viewer).await;
    };
    let archived = params
        .iter()
        .any(|(key, value)| key == "archived" && value == "1");
    if let Some(redirect) = redirect_unless_canonical(&uri, &index_href(sort, archived)) {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let spaces = content::space_index(state.db(), &viewer, sort, archived, &mut deps).await?;
    cacheable(
        &SpacesTemplate {
            layout: Layout::load(&state, &viewer, Nav::Spaces).await?,
            spaces,
            sort,
            archived,
            can_create: may_create(&state, &viewer).await?,
        },
        &deps,
    )
}

// ---------------------------------------------------------------------------
// Making a space

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct NewSpaceForm {
    name: String,
    slug: String,
    description: String,
}

#[derive(Template)]
#[template(path = "space_new.html")]
struct NewSpaceTemplate {
    layout: Layout,
    errors: Vec<String>,
    form: NewSpaceForm,
}

async fn render_new(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    form: NewSpaceForm,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    render(
        status,
        &NewSpaceTemplate {
            layout: Layout::load(state, viewer, Nav::Spaces).await?,
            errors,
            form,
        },
    )
}

/// The member who may make a space now, or the page for whoever may not.
async fn creator<'v>(
    state: &AppState,
    viewer: &'v Viewer,
) -> Result<Result<&'v UserCtx, Response>, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(Err(web::login_first("/spaces/new")));
    };
    if !may_create(state, viewer).await? {
        return Ok(Err(web::forbidden(state, viewer).await?));
    }
    Ok(Ok(user))
}

pub async fn new_page(State(state): State<AppState>, viewer: Viewer) -> Result<Response, AppError> {
    if let Err(response) = creator(&state, &viewer).await? {
        return Ok(response);
    }
    render_new(
        &state,
        &viewer,
        StatusCode::OK,
        NewSpaceForm::default(),
        Vec::new(),
    )
    .await
}

pub async fn create(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<NewSpaceForm>,
) -> Result<Response, AppError> {
    let user = match creator(&state, &viewer).await? {
        Ok(user) => user,
        Err(response) => return Ok(response),
    };
    let settings = SpaceSettings {
        slug: form.slug.clone(),
        name: form.name.clone(),
        description: form.description.clone(),
        administer: Grant::listed([], [user.id]),
        ..SpaceSettings::default()
    };
    let settings = match settings.clean() {
        Ok(settings) => settings,
        Err(problems) => {
            return render_new(&state, &viewer, StatusCode::BAD_REQUEST, form, problems).await;
        }
    };
    if !state
        .limiter()
        .check(Action::CreateSpace, Scope::User, &user.id.to_string())
    {
        return web::rate_limited(&state, &viewer).await;
    }
    match store::add_space(state.db(), &settings, Some(user.id)).await? {
        Err(why) => render_new(&state, &viewer, StatusCode::BAD_REQUEST, form, vec![why]).await,
        Ok(space_id) => {
            state.changed(Change::SpaceChanged { space_id }).await?;
            Ok(see_other(&format!(
                "/c/{}/settings?done=created",
                settings.slug
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// A space's settings

/// One grant, as typed into the form.
#[derive(Clone, Debug, Default)]
pub struct GrantForm {
    level: String,
    groups: Vec<String>,
    /// Usernames, separated by commas or spaces.
    users: String,
}

impl GrantForm {
    fn from_grant(grant: &Grant, usernames: &HashMap<i64, String>) -> GrantForm {
        GrantForm {
            level: grant.level.as_str().to_owned(),
            groups: grant.group_ids.iter().map(i64::to_string).collect(),
            users: grant
                .user_ids
                .iter()
                .filter_map(|id| usernames.get(id))
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    fn ticked(&self, group_id: &i64) -> bool {
        self.groups.contains(&group_id.to_string())
    }
}

/// The settings form, as typed.
#[derive(Clone, Debug, Default)]
pub struct SettingsForm {
    name: String,
    slug: String,
    description: String,
    section: String,
    position: String,
    read: GrantForm,
    reply: GrantForm,
    start: GrantForm,
    administer: GrantForm,
}

impl SettingsForm {
    fn grant_mut(&mut self, verb: &str) -> Option<&mut GrantForm> {
        match verb {
            "read" => Some(&mut self.read),
            "reply" => Some(&mut self.reply),
            "start" => Some(&mut self.start),
            "administer" => Some(&mut self.administer),
            _ => None,
        }
    }

    fn from_fields(fields: &[(String, String)]) -> SettingsForm {
        let mut form = SettingsForm::default();
        for (key, value) in fields {
            match key.as_str() {
                "name" => form.name = value.clone(),
                "slug" => form.slug = value.clone(),
                "description" => form.description = value.clone(),
                "section" => form.section = value.clone(),
                "position" => form.position = value.clone(),
                _ => {
                    if let Some((verb, part)) = key.rsplit_once('_')
                        && let Some(grant) = form.grant_mut(verb)
                    {
                        match part {
                            "level" => grant.level = value.clone(),
                            "group" => grant.groups.push(value.clone()),
                            "users" => grant.users = value.clone(),
                            _ => {}
                        }
                    }
                }
            }
        }
        form
    }

    async fn from_settings(
        state: &AppState,
        settings: &SpaceSettings,
    ) -> Result<SettingsForm, AppError> {
        let ids: Vec<i64> = [
            &settings.read,
            &settings.reply,
            &settings.start,
            &settings.administer,
        ]
        .iter()
        .flat_map(|grant| grant.user_ids())
        .collect();
        let usernames: HashMap<i64, String> = users::usernames_of(state.db(), &ids)
            .await?
            .into_iter()
            .map(|user| (user.id, user.username))
            .collect();
        Ok(SettingsForm {
            name: settings.name.clone(),
            slug: settings.slug.clone(),
            description: settings.description.clone(),
            section: settings
                .section_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            position: settings.position.to_string(),
            read: GrantForm::from_grant(&settings.read, &usernames),
            reply: GrantForm::from_grant(&settings.reply, &usernames),
            start: GrantForm::from_grant(&settings.start, &usernames),
            administer: GrantForm::from_grant(&settings.administer, &usernames),
        })
    }

    /// The settings this form describes, starting from the space's
    /// current ones: the section and position change only for staff.
    async fn settings(
        &self,
        state: &AppState,
        current: &SpaceSettings,
        staff: bool,
        sections: &[Section],
        groups: &[GroupSummary],
    ) -> Result<Result<SpaceSettings, Vec<String>>, AppError> {
        let mut problems = Vec::new();
        let (section_id, position) = if staff {
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
            let position = match self.position.trim() {
                "" => 0,
                text => text.parse::<i32>().unwrap_or_else(|_| {
                    problems.push("The position should be a whole number, such as 3.".to_owned());
                    0
                }),
            };
            (section_id, position)
        } else {
            (current.section_id, current.position)
        };
        let mut grants = Vec::new();
        for (verb, form, what) in [
            ("read", &self.read, "read the space"),
            ("reply", &self.reply, "reply"),
            ("start", &self.start, "start threads"),
            ("administer", &self.administer, "administer the space"),
        ] {
            let Some(level) = Level::parse(form.level.trim()) else {
                problems.push(format!("Choose who can {what}."));
                grants.push((verb, Grant::staff()));
                continue;
            };
            if level != Level::Listed {
                grants.push((verb, Grant::from_parts(level.as_str(), &[], &[])));
                continue;
            }
            let group_ids: Vec<i64> = form
                .groups
                .iter()
                .filter_map(|id| id.trim().parse::<i64>().ok())
                .filter(|id| groups.iter().any(|g| g.id == *id))
                .collect();
            if group_ids.len() != form.groups.len() {
                problems.push("Choose groups from the list.".to_owned());
            }
            let names = crate::messages::parse_recipients(&form.users);
            let found = users::by_usernames(state.db(), &names).await?;
            for name in &names {
                if !found.iter().any(|u| u.username.eq_ignore_ascii_case(name)) {
                    problems.push(format!("There is no member called {name}."));
                }
            }
            grants.push((verb, Grant::listed(group_ids, found.iter().map(|u| u.id))));
        }
        problems.dedup();
        let grant = |verb: &str| {
            grants
                .iter()
                .find(|(v, _)| *v == verb)
                .map(|(_, grant)| grant.clone())
                .expect("every verb has a grant")
        };
        let settings = SpaceSettings {
            section_id,
            slug: self.slug.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            position,
            read: grant("read"),
            reply: grant("reply"),
            start: grant("start"),
            administer: grant("administer"),
        }
        .clean();
        Ok(match settings {
            Ok(settings) if problems.is_empty() => Ok(settings),
            Ok(_) => Err(problems),
            Err(more) => {
                problems.extend(more);
                Err(problems)
            }
        })
    }
}

#[derive(Template)]
#[template(path = "space_settings.html")]
struct SettingsTemplate {
    layout: Layout,
    space: SpaceTarget,
    notice: Option<&'static str>,
    errors: Vec<String>,
    form: SettingsForm,
    sections: Vec<Section>,
    groups: Vec<GroupSummary>,
    /// Staff also place the space in a section.
    staff: bool,
    /// How many threads it has ever had: only one with none can be deleted.
    thread_count: i64,
}

impl SettingsTemplate {
    fn section_chosen(&self, id: &i64) -> bool {
        self.form.section.trim() == id.to_string()
    }
}

/// The space at `slug` if the viewer administers it, or the page for
/// whoever doesn't.
async fn administered(
    state: &AppState,
    viewer: &Viewer,
    slug: &str,
) -> Result<Result<SpaceTarget, Response>, AppError> {
    if viewer.user().is_none() {
        return Ok(Err(web::login_first(&format!("/c/{slug}/settings"))));
    }
    let Some(space) = content::space_target(state.db(), viewer, slug).await? else {
        return Ok(Err(web::not_found(state, viewer).await?));
    };
    if !space.rules.may_administer(viewer) {
        return Ok(Err(web::forbidden(state, viewer).await?));
    }
    Ok(Ok(space))
}

async fn render_settings(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    space: SpaceTarget,
    form: SettingsForm,
    notice: Option<&'static str>,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    let db = state.db();
    render(
        status,
        &SettingsTemplate {
            layout: Layout::load(state, viewer, Nav::None).await?,
            thread_count: content::space_thread_count(db, viewer, space.space.id).await?,
            sections: content::sections(db).await?,
            groups: groups::list(db).await?,
            staff: viewer.is_staff(),
            space,
            notice,
            errors,
            form,
        },
    )
}

pub async fn settings_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    let space = match administered(&state, &viewer, &slug).await? {
        Ok(space) => space,
        Err(response) => return Ok(response),
    };
    let notice = match query.done.as_str() {
        "created" => Some(
            "The space is ready. To start with, any member can read it, reply and start threads; change that here.",
        ),
        "saved" => Some("Saved."),
        _ => None,
    };
    let form = SettingsForm::from_settings(&state, &space.settings()).await?;
    render_settings(
        &state,
        &viewer,
        StatusCode::OK,
        space,
        form,
        notice,
        Vec::new(),
    )
    .await
}

/// Records what a staff member did to a space they don't administer in
/// their own right (DESIGN.md §3.1).
async fn log_staff_action(
    state: &AppState,
    viewer: &Viewer,
    space: &SpaceTarget,
    action: &str,
) -> Result<(), AppError> {
    if !viewer.is_staff() || space.rules.administers_by_grant(viewer) {
        return Ok(());
    }
    let Some(user) = viewer.user() else {
        return Ok(());
    };
    mod_records::record(
        state.db(),
        &NewAction {
            actor_id: user.id,
            action,
            target_kind: "space",
            target_id: space.space.id,
            user_id: None,
            thread_id: None,
            post_id: None,
            reason: "",
            public: space.visibility == "public",
            details: json!({ "slug": space.space.slug, "name": space.space.name }),
        },
    )
    .await?;
    state.changed(Change::ModLogged).await?;
    Ok(())
}

pub async fn save_settings(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    let space = match administered(&state, &viewer, &slug).await? {
        Ok(space) => space,
        Err(response) => return Ok(response),
    };
    let db = state.db();
    let form = SettingsForm::from_fields(&fields);
    let current = space.settings();
    let settings = form
        .settings(
            &state,
            &current,
            viewer.is_staff(),
            &content::sections(db).await?,
            &groups::list(db).await?,
        )
        .await?;
    let settings = match settings {
        Ok(settings) => settings,
        Err(problems) => {
            return render_settings(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                space,
                form,
                None,
                problems,
            )
            .await;
        }
    };
    match store::update_space(db, space.space.id, &settings).await? {
        Err(why) => {
            render_settings(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                space,
                form,
                None,
                vec![why],
            )
            .await
        }
        Ok(false) => web::not_found(&state, &viewer).await,
        Ok(true) => {
            log_staff_action(&state, &viewer, &space, "edit_space").await?;
            // A new read grant changes every page that could show the
            // space's threads; this clears them all.
            state
                .changed(Change::SpaceChanged {
                    space_id: space.space.id,
                })
                .await?;
            Ok(see_other(&format!(
                "/c/{}/settings?done=saved",
                settings.slug
            )))
        }
    }
}

async fn set_archived(
    state: &AppState,
    viewer: &Viewer,
    slug: &str,
    archived: bool,
) -> Result<Response, AppError> {
    let space = match administered(state, viewer, slug).await? {
        Ok(space) => space,
        Err(response) => return Ok(response),
    };
    if store::set_archived(state.db(), space.space.id, archived).await? {
        let action = if archived {
            "archive_space"
        } else {
            "unarchive_space"
        };
        log_staff_action(state, viewer, &space, action).await?;
        state
            .changed(Change::SpaceChanged {
                space_id: space.space.id,
            })
            .await?;
    }
    Ok(see_other(&format!("/c/{}", space.space.slug)))
}

pub async fn archive(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    set_archived(&state, &viewer, &slug, true).await
}

pub async fn unarchive(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    set_archived(&state, &viewer, &slug, false).await
}

pub async fn delete(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    let space = match administered(&state, &viewer, &slug).await? {
        Ok(space) => space,
        Err(response) => return Ok(response),
    };
    match store::delete_space(state.db(), space.space.id).await? {
        Ok(true) => {
            log_staff_action(&state, &viewer, &space, "delete_space").await?;
            state
                .changed(Change::SpaceChanged {
                    space_id: space.space.id,
                })
                .await?;
            Ok(see_other("/spaces"))
        }
        Ok(false) => web::not_found(&state, &viewer).await,
        Err(why) => {
            let form = SettingsForm::from_settings(&state, &space.settings()).await?;
            render_settings(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                space,
                form,
                None,
                vec![why],
            )
            .await
        }
    }
}
