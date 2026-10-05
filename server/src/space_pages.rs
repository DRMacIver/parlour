//! Spaces as members make and run them (DESIGN.md §3.1): the space index
//! at `/spaces`, making one at `/spaces/new`, and a space's settings,
//! archiving and deletion at `/c/{slug}/settings` and its neighbours.
//!
//! Anyone who administers a space (its administer grant names them, or
//! they are staff) may change it. Staff changing a space they don't
//! administer in their own right is recorded in the moderation log. Staff
//! also place the space on the default front page from here, and admins
//! mark it essential (DESIGN.md §3.1, §3.5).
//!
//! `/c/{slug}/audience` tells members who can read a space who may read,
//! reply, start threads in and run it, naming lists only as DESIGN.md §3.3
//! allows.
//!
//! Members arrange their own front page from a space's page too: `POST
//! /c/{slug}/follow` puts it in a section of their layout, `unfollow`
//! takes it out, `mute` leaves it out of All, the space index and search,
//! and `unmute` undoes that (DESIGN.md §3.5). None of these changes what
//! anyone else sees, so nothing cached is cleared.

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
use crate::content::{
    self, Deps, FeedOwner, GrantAudience, KindFilter, NamedGroup, SpaceAudience, SpaceListing,
    SpaceSort, SpaceTarget,
};
use crate::error::AppError;
use crate::format;
use crate::groups::{self, GroupChoice};
use crate::layouts::{self, MuteOutcome, Section};
use crate::mod_records::{self, NewAction};
use crate::pages::well_formed_slug;
use crate::permissions::{Grant, Level, user_can_read};
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
fn index_href(sort: SpaceSort, kinds: KindFilter, archived: bool, muted: bool) -> String {
    let mut href = String::from("/spaces");
    let mut sep = '?';
    if sort != SpaceSort::Created {
        href.push(sep);
        href.push_str("sort=");
        href.push_str(sort.as_str());
        sep = '&';
    }
    if kinds != KindFilter::Both {
        href.push(sep);
        href.push_str("kind=");
        href.push_str(kinds.as_str());
        sep = '&';
    }
    if archived {
        href.push(sep);
        href.push_str("archived=1");
        sep = '&';
    }
    if muted {
        href.push(sep);
        href.push_str("muted=1");
    }
    href
}

#[derive(Template)]
#[template(path = "spaces.html")]
struct SpacesTemplate {
    layout: Layout,
    spaces: Vec<SpaceListing>,
    sort: SpaceSort,
    kinds: KindFilter,
    archived: bool,
    /// Whether the spaces the member has muted are shown too.
    muted: bool,
    /// Whether the member has muted anything, so that the toggle is
    /// worth offering.
    has_mutes: bool,
    can_create: bool,
    /// What was just done, after deleting a space.
    notice: Option<&'static str>,
}

impl SpacesTemplate {
    fn sort_is(&self, sort: &str) -> bool {
        self.sort.as_str() == sort
    }

    fn sort_href(&self, sort: &str) -> String {
        index_href(
            SpaceSort::parse(sort).unwrap_or(SpaceSort::Created),
            self.kinds,
            self.archived,
            self.muted,
        )
    }

    fn kind_is(&self, kinds: &str) -> bool {
        self.kinds.as_str() == kinds
    }

    fn kind_href(&self, kinds: &str) -> String {
        index_href(
            self.sort,
            KindFilter::parse(kinds).unwrap_or(KindFilter::Both),
            self.archived,
            self.muted,
        )
    }

    fn archived_href(&self) -> String {
        index_href(self.sort, self.kinds, !self.archived, self.muted)
    }

    fn muted_href(&self) -> String {
        index_href(self.sort, self.kinds, self.archived, !self.muted)
    }
}

pub async fn index(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    let Query(params) = Query::<Vec<(String, String)>>::try_from_uri(&uri).unwrap_or_default();
    // Each of these may be given once, or left out for its default.
    let one = |name: &str| {
        let mut values = params.iter().filter(|(key, _)| key == name);
        match (values.next(), values.next()) {
            (None, _) => Ok(None),
            (Some((_, value)), None) => Ok(Some(value.as_str())),
            (Some(_), Some(_)) => Err(()),
        }
    };
    let sort = match one("sort") {
        Ok(None) => Some(SpaceSort::Created),
        Ok(Some(value)) => SpaceSort::parse(value),
        Err(()) => None,
    };
    let kinds = match one("kind") {
        Ok(None) => Some(KindFilter::Both),
        Ok(Some(value)) => KindFilter::parse(value),
        Err(()) => None,
    };
    let (Some(sort), Some(kinds)) = (sort, kinds) else {
        return web::not_found(&state, &viewer).await;
    };
    let flag = |name: &str| {
        params
            .iter()
            .any(|(key, value)| key == name && value == "1")
    };
    let archived = flag("archived");
    // Visitors have nothing muted, so for them this is no view at all; nor
    // have they deleted anything.
    let muted = flag("muted") && viewer.user().is_some();
    let deleted = flag("deleted") && viewer.user().is_some();
    let mut canonical = index_href(sort, kinds, archived, muted);
    if deleted {
        canonical.push(if canonical.contains('?') { '&' } else { '?' });
        canonical.push_str("deleted=1");
    }
    if let Some(redirect) = redirect_unless_canonical(&uri, &canonical) {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let spaces =
        content::space_index(state.db(), &viewer, sort, kinds, archived, muted, &mut deps).await?;
    let has_mutes = match viewer.user() {
        Some(user) => layouts::has_mutes(state.db(), user.id).await?,
        None => false,
    };
    cacheable(
        &SpacesTemplate {
            layout: Layout::load(&state, &viewer, Nav::Spaces).await?,
            spaces,
            sort,
            kinds,
            archived,
            muted,
            has_mutes,
            can_create: may_create(&state, &viewer).await?,
            notice: deleted.then_some("The space was deleted."),
        },
        &deps,
    )
}

// ---------------------------------------------------------------------------
// Following and muting

/// The member acting on a space they can read at `slug`, or the page for
/// whoever can't: visitors log in first, and a suspended member can do
/// nothing but contest it.
async fn member_and_space<'v>(
    state: &AppState,
    viewer: &'v Viewer,
    slug: &str,
) -> Result<Result<(&'v UserCtx, SpaceTarget), Response>, AppError> {
    let Some(user) = viewer.user() else {
        return Ok(Err(web::login_first(&format!("/c/{slug}"))));
    };
    let Some(space) = content::space_target(state.db(), viewer, slug).await? else {
        return Ok(Err(web::not_found(state, viewer).await?));
    };
    if !user.may_act() {
        return Ok(Err(web::forbidden(state, viewer).await?));
    }
    Ok(Ok((user, space)))
}

/// Puts the space in a section of the member's layout: the first, or a
/// new one called "Following" if there are none.
pub async fn follow(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    let (user, space) = match member_and_space(&state, &viewer, &slug).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let layout = layouts::own_id(state.db(), user.id).await?;
    if !layouts::follow_in_first_section(state.db(), layout, space.space.id).await? {
        return web::not_found(&state, &viewer).await;
    }
    Ok(see_other(&format!("/c/{}", space.space.slug)))
}

/// Takes the space out of the member's sections.
pub async fn unfollow(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    let (user, space) = match member_and_space(&state, &viewer, &slug).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let layout = layouts::own_id(state.db(), user.id).await?;
    layouts::unfollow(state.db(), layout, space.space.id).await?;
    Ok(see_other(&format!("/c/{}", space.space.slug)))
}

/// Mutes the space for the member (DESIGN.md §3.5), unless it is
/// essential.
pub async fn mute(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    let (user, space) = match member_and_space(&state, &viewer, &slug).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let back = format!("/c/{}", space.space.slug);
    match layouts::mute(state.db(), user.id, space.space.id).await? {
        MuteOutcome::Muted | MuteOutcome::AlreadyMuted => Ok(see_other(&back)),
        MuteOutcome::Essential => {
            web::message(
                &state,
                &viewer,
                StatusCode::FORBIDDEN,
                "Not allowed",
                &["This space is essential: it is on everyone’s front page and can’t be muted."],
                Some((&back, "Back to the space")),
            )
            .await
        }
        MuteOutcome::NoSuchSpace => web::not_found(&state, &viewer).await,
    }
}

/// Unmutes the space, then goes back to it, or to `next` (a path on this
/// site) if the form says: the layout editor's list of mutes comes back
/// to itself.
pub async fn unmute(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    let (user, space) = match member_and_space(&state, &viewer, &slug).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    layouts::unmute(state.db(), user.id, space.space.id).await?;
    let back = format!("/c/{}", space.space.slug);
    let next = fields
        .iter()
        .find(|(key, _)| key == "next")
        .and_then(|(_, value)| web::safe_next(value));
    Ok(see_other(next.unwrap_or(&back)))
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

    /// Whether a choice is ticked: by its value, as the form sends it
    /// back, or for a saved grant by the groups it stands for.
    fn ticked(&self, choice: &GroupChoice) -> bool {
        self.groups.iter().any(|value| {
            value.trim() == choice.value || choice.ids.iter().any(|id| *value == id.to_string())
        })
    }
}

/// The settings form, as typed.
#[derive(Clone, Debug, Default)]
pub struct SettingsForm {
    name: String,
    slug: String,
    description: String,
    /// The default front page section to put it in (staff); empty for
    /// none.
    section: String,
    /// Whether the essential box was ticked (admins).
    essential: bool,
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
                "essential" => form.essential = true,
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

    async fn from_space(state: &AppState, space: &SpaceTarget) -> Result<SettingsForm, AppError> {
        let settings = space.settings();
        let db = state.db();
        let default = layouts::default_id(db).await?;
        let section = layouts::section_of(db, default, space.space.id).await?;
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
        // A feed's owner holds all of its permissions, without being
        // named in the form.
        let unnamed = |grant: &Grant| {
            let mut grant = grant.clone();
            if let Some(owner) = &space.owner {
                grant.user_ids.remove(&owner.user.id);
            }
            grant
        };
        Ok(SettingsForm {
            name: settings.name.clone(),
            slug: settings.slug.clone(),
            description: settings.description.clone(),
            section: section.map(|id| id.to_string()).unwrap_or_default(),
            essential: space.essential,
            read: GrantForm::from_grant(&unnamed(&settings.read), &usernames),
            reply: GrantForm::from_grant(&unnamed(&settings.reply), &usernames),
            start: GrantForm::from_grant(&unnamed(&settings.start), &usernames),
            administer: GrantForm::from_grant(&unnamed(&settings.administer), &usernames),
        })
    }

    /// The default front page section the form asks for, given the
    /// default layout's sections: `None` for none.
    fn section(&self, sections: &[Section]) -> Result<Option<i64>, String> {
        match self.section.trim() {
            "" => Ok(None),
            id => match id.parse::<i64>() {
                Ok(id) if sections.iter().any(|s| s.id == id) => Ok(Some(id)),
                _ => Err("Choose a section from the list.".to_owned()),
            },
        }
    }

    /// The settings this form describes.
    async fn settings(
        &self,
        state: &AppState,
        groups: &GrantChoices,
    ) -> Result<Result<SpaceSettings, Vec<String>>, AppError> {
        let mut problems = Vec::new();
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
            let group_ids =
                groups::chosen(groups.for_verb(verb), &form.groups).unwrap_or_else(|| {
                    problems.push("Choose groups from the list.".to_owned());
                    Vec::new()
                });
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
            slug: self.slug.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
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
    /// The default front page's sections, for staff to choose from.
    sections: Vec<Section>,
    /// The groups and lists the viewer may name, per verb (DESIGN.md §3.3).
    groups: GrantChoices,
    /// Staff also place the space on the default front page.
    staff: bool,
    /// Admins also mark it essential.
    admin: bool,
    /// How many threads it has ever had: only one with none can be deleted.
    thread_count: i64,
    /// Whether it is the viewer's own feed.
    own_feed: bool,
    /// Whether the viewer runs it as staff without being able to read it,
    /// so may break glass on it from here (DESIGN.md §8).
    closed: bool,
    /// For staff, whether nobody runs it, and when its maker was last seen
    /// (DESIGN.md §3.1).
    unrun: Option<content::Unrun>,
    /// The handing-on form's fields, if refused.
    hand: HandForm,
}

/// The form for handing on a space nobody runs.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct HandForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    reason: String,
}

impl SettingsTemplate {
    fn section_chosen(&self, id: &i64) -> bool {
        self.form.section.trim() == id.to_string()
    }

    /// Whether the space is a feed whose owner has deleted their account,
    /// which stays archived (DESIGN.md §3.2).
    fn owner_gone(&self) -> bool {
        self.space.owner.as_ref().is_some_and(|owner| owner.deleted)
    }

    /// What a feed's form says under reading, replying and starting
    /// threads: that its owner always can, then `base`.
    fn feed_hint(&self, owner: &FeedOwner, base: &str) -> String {
        if self.own_feed {
            format!("You always can. {base}")
        } else {
            format!("{} always can. {base}", owner.user.display_name)
        }
    }

    /// What the settings say to staff about a space nobody runs.
    fn unrun_note(&self) -> String {
        let Some(unrun) = &self.unrun else {
            return String::new();
        };
        let maker = match (&unrun.maker, unrun.days_away()) {
            (Some(maker), Some(days)) => format!(
                "{}, who made it, was last seen {} ago",
                maker.display_name,
                crate::format::plural(days, "day", "days")
            ),
            _ => "Whoever made it has left the site".to_owned(),
        };
        if unrun.claimable() {
            format!(
                "{maker}, so a member who asks to run it can be handed it here. Make sure they can read it."
            )
        } else {
            let wait = content::CLAIM_DAYS - unrun.days_away().unwrap_or_default();
            format!(
                "{maker}. A member who asks to run it can be handed it once its maker has been away for {} days, in {}.",
                content::CLAIM_DAYS,
                crate::format::plural(wait, "day", "days")
            )
        }
    }

    /// What a feed's form says about who runs it.
    fn feed_runner_hint(&self, owner: &FeedOwner) -> String {
        let also = "Others named here change these settings, pin, lock, rename and delete its threads, hide posts in it, and can archive it, as moderators and admins always can.";
        if self.own_feed {
            format!("You always run your feed. {also}")
        } else {
            format!("{} always runs their feed. {also}", owner.user.display_name)
        }
    }
}

/// The sections of the default front page, in order.
async fn default_sections(state: &AppState) -> Result<Vec<Section>, AppError> {
    let db = state.db();
    let default = layouts::default_id(db).await?;
    Ok(layouts::read(db, default)
        .await?
        .map(|layout| layout.sections)
        .unwrap_or_default())
}

/// What each of a space's grant forms offers (DESIGN.md §3.3).
struct GrantChoices {
    read: Vec<GroupChoice>,
    reply: Vec<GroupChoice>,
    start: Vec<GroupChoice>,
    administer: Vec<GroupChoice>,
}

impl GrantChoices {
    fn for_verb(&self, verb: &str) -> &[GroupChoice] {
        match verb {
            "read" => &self.read,
            "reply" => &self.reply,
            "start" => &self.start,
            _ => &self.administer,
        }
    }
}

/// The groups and lists each of the space's grant forms offers the
/// viewer: the site's groups and the lists they own or are on, in any of
/// them, and whatever other lists a grant names already, in that grant
/// only, kept without being explained to them. Nobody else can put a list
/// where it wasn't.
async fn choosable(
    state: &AppState,
    viewer: &Viewer,
    space: &SpaceTarget,
) -> Result<GrantChoices, AppError> {
    let rules = &space.rules;
    let offered = async |grant: &Grant| {
        groups::choosable(state.db(), viewer.user_id(), &grant.group_ids()).await
    };
    Ok(GrantChoices {
        read: offered(&rules.read).await?,
        reply: offered(&rules.reply).await?,
        start: offered(&rules.start).await?,
        administer: offered(&rules.administer).await?,
    })
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
    // Staff run every space, whether or not they can read it (DESIGN.md
    // §3.1); its settings show nothing of its threads.
    let Some(space) = content::administered_space(state.db(), viewer, slug).await? else {
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
    render_settings_with(
        state,
        viewer,
        status,
        space,
        form,
        notice,
        errors,
        HandForm::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn render_settings_with(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    space: SpaceTarget,
    form: SettingsForm,
    notice: Option<&'static str>,
    errors: Vec<String>,
    hand: HandForm,
) -> Result<Response, AppError> {
    let db = state.db();
    render(
        status,
        &SettingsTemplate {
            layout: Layout::load(state, viewer, Nav::None).await?,
            unrun: content::unrun_space(db, viewer, space.space.id).await?,
            hand,
            thread_count: content::space_thread_count(db, viewer, space.space.id).await?,
            sections: default_sections(state).await?,
            groups: choosable(state, viewer, &space).await?,
            staff: viewer.is_staff(),
            admin: viewer.is_admin(),
            own_feed: space.owned_by(viewer),
            closed: viewer.is_staff()
                && viewer
                    .user()
                    .is_some_and(|user| !user_can_read(user, &space.rules.read.audience())),
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
        "handed" => Some("Handed on: they run it now."),
        _ => None,
    };
    let form = SettingsForm::from_space(&state, &space).await?;
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
    if !viewer.is_staff() || space.rules.runs_in_own_right(viewer) {
        return Ok(());
    }
    let Some(user) = viewer.user() else {
        return Ok(());
    };
    record_space_action(state, user.id, space, action).await
}

/// Records something a member of staff did to a space in the moderation
/// log, public if the space is.
pub(crate) async fn record_space_action(
    state: &AppState,
    actor_id: i64,
    space: &SpaceTarget,
    action: &str,
) -> Result<(), AppError> {
    mod_records::record(
        state.db(),
        &NewAction {
            actor_id,
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
    let mut form = SettingsForm::from_fields(&fields);
    // A feed's address is its owner's username, which its form doesn't
    // offer to change (DESIGN.md §3.2).
    if space.is_feed() && !fields.iter().any(|(key, _)| key == "slug") {
        form.slug = space.space.slug.clone();
    }
    let mut settings = form
        .settings(&state, &choosable(&state, &viewer, &space).await?)
        .await?;
    // Staff run spaces they can't read, but changing who may read one so
    // that they could is reading it by another way: breaking glass, which
    // they confirm, saying why (DESIGN.md §3.1, §8). Naming themselves
    // among those who run it changes nothing: anyone else the administer
    // grant names must read the space to run it, so a member of staff who
    // can't read it runs it only by their role. Those told are those who
    // could read it before the change.
    let mut glass = None;
    if let (Some(user), Ok(new)) = (viewer.user(), &settings)
        && viewer.is_staff()
        && !user_can_read(user, &space.rules.read.audience())
        && user_can_read(user, &new.read.audience())
    {
        let opens: Vec<_> = content::access::closed(
            db,
            &viewer,
            crate::staff_access::Scope::Space(space.space.id),
        )
        .await?
        .into_iter()
        .collect();
        let action = format!("/c/{}/settings", space.space.slug);
        match crate::glass_pages::ask_change(
            &state,
            &viewer,
            opens.clone(),
            crate::glass_pages::Change {
                action: &action,
                fields: &fields,
                says: format!(
                    "You can’t read {}, and saving these settings would let you.",
                    space.space.name
                ),
                button: "Break glass and save",
                back: &action,
                by_change: true,
            },
        )
        .await?
        {
            crate::glass_pages::Confirmation::Ask(response) => return Ok(response),
            crate::glass_pages::Confirmation::Given(reason) => {
                glass = Some((reason, crate::glass::readers(&state, user, &opens).await?));
            }
        }
    }
    // Where it goes on the default front page is for staff to say.
    let section = if viewer.is_staff() {
        match form.section(&default_sections(&state).await?) {
            Ok(section) => Some(section),
            Err(why) => {
                let mut problems = settings.err().unwrap_or_default();
                problems.push(why);
                settings = Err(problems);
                None
            }
        }
    } else {
        None
    };
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
    // Breaking glass, the change is made with its grants and notices or not
    // at all (DESIGN.md §8).
    let mut tx = db.begin().await?;
    let saved = store::update_space(&mut tx, space.space.id, &settings).await?;
    let recorded = match (&saved, viewer.user(), &glass) {
        (Ok(true), Some(user), Some((reason, readers))) => {
            Some(crate::glass::record(&mut tx, user, readers, reason, None, true).await?)
        }
        _ => None,
    };
    tx.commit().await?;
    match saved {
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
            if let Some(recorded) = recorded {
                recorded.announce(&state).await?;
            }
            if let Some(section) = section {
                place_on_default(&state, space.space.id, section).await?;
            }
            if viewer.is_admin() && form.essential != space.essential {
                layouts::set_essential(db, space.space.id, form.essential).await?;
                let action = if form.essential {
                    "mark_space_essential"
                } else {
                    "unmark_space_essential"
                };
                // Only admins mark spaces essential, as admins and never as
                // the space's administrators, so this is always logged.
                if let Some(user) = viewer.user() {
                    record_space_action(&state, user.id, &space, action).await?;
                }
            }
            log_staff_action(&state, &viewer, &space, "edit_space").await?;
            // A new read grant changes every page that could show the
            // space's threads, and so does a place on the default front
            // page; this clears them all.
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

/// Puts the space in a section of the default front page, or (`None`)
/// takes it off, if that isn't where it is already.
async fn place_on_default(
    state: &AppState,
    space_id: i64,
    section: Option<i64>,
) -> Result<(), AppError> {
    let db = state.db();
    let default = layouts::default_id(db).await?;
    if layouts::section_of(db, default, space_id).await? == section {
        return Ok(());
    }
    match section {
        Some(section) => {
            layouts::follow(db, default, section, space_id).await?;
        }
        None => {
            layouts::unfollow(db, default, space_id).await?;
        }
    }
    Ok(())
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

/// `POST /c/{slug}/hand`: staff hand a topic space nobody runs, whose
/// maker has been away for [`content::CLAIM_DAYS`] days, to a member who
/// asked to run it (DESIGN.md §3.1). It is logged, and they are told.
pub async fn hand(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
    Form(hand): Form<HandForm>,
) -> Result<Response, AppError> {
    let space = match administered(&state, &viewer, &slug).await? {
        Ok(space) => space,
        Err(response) => return Ok(response),
    };
    let Some(user) = viewer.user().filter(|_| viewer.is_staff()) else {
        return web::forbidden(&state, &viewer).await;
    };
    let db = state.db();
    let reason = crate::moderation::clean_line(&hand.reason);
    let mut problems = Vec::new();
    match content::unrun_space(db, &viewer, space.space.id).await? {
        None => problems.push(
            "Someone runs this space already. Change who runs it in the settings above instead."
                .to_owned(),
        ),
        Some(unrun) if !unrun.claimable() => problems.push(format!(
            "Its maker hasn’t been away for {} days yet.",
            content::CLAIM_DAYS
        )),
        Some(_) => {}
    }
    let found = users::by_usernames(db, &[hand.username.trim().to_owned()]).await?;
    let member = match found.first() {
        None => {
            problems.push(format!(
                "There is no member called {}.",
                hand.username.trim()
            ));
            None
        }
        Some(found) => crate::sessions::user_ctx(db, found.id).await?,
    };
    if let Some(member) = &member
        && (member.is_suspended() || !user_can_read(member, &space.rules.read.audience()))
    {
        problems.push(format!(
            "{} can’t read this space, so couldn’t run it.",
            member.display_name
        ));
    }
    crate::moderation::check_reason(&reason, &mut problems);
    let member = match member {
        Some(member) if problems.is_empty() => member,
        _ => {
            let form = SettingsForm::from_space(&state, &space).await?;
            return render_settings_with(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                space,
                form,
                None,
                problems,
                hand,
            )
            .await;
        }
    };
    store::set_grant(
        db,
        space.space.id,
        "administer",
        &Grant::listed([], [member.id]),
    )
    .await?;
    let action_id = mod_records::record(
        db,
        &NewAction {
            actor_id: user.id,
            action: "hand_space",
            target_kind: "space",
            target_id: space.space.id,
            user_id: Some(member.id),
            thread_id: None,
            post_id: None,
            reason: &reason,
            public: space.visibility == "public",
            details: json!({ "slug": space.space.slug, "name": space.space.name }),
        },
    )
    .await?;
    crate::notify::moderation(
        &state,
        member.id,
        Some(user.id),
        &format!(
            "A moderator handed you the space {}, which nobody ran any more: you run it now.",
            space.space.name
        ),
        Some(action_id),
    )
    .await?;
    state.changed(Change::ModLogged).await?;
    state
        .changed(Change::SpaceChanged {
            space_id: space.space.id,
        })
        .await?;
    Ok(see_other(&format!(
        "/c/{}/settings?done=handed",
        space.space.slug
    )))
}

#[derive(Template)]
#[template(path = "space_delete.html")]
struct DeleteTemplate {
    layout: Layout,
    space: SpaceTarget,
    /// How many threads it has ever had: only one with none can be deleted.
    thread_count: i64,
}

/// `GET /c/{slug}/delete`: asks those who run the space whether to delete
/// it, as deleting a section or a group does.
pub async fn delete_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    let space = match administered(&state, &viewer, &slug).await? {
        Ok(space) => space,
        Err(response) => return Ok(response),
    };
    render(
        StatusCode::OK,
        &DeleteTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            thread_count: content::space_thread_count(state.db(), &viewer, space.space.id).await?,
            space,
        },
    )
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
            Ok(see_other("/spaces?deleted=1"))
        }
        Ok(false) => web::not_found(&state, &viewer).await,
        Err(why) => {
            let form = SettingsForm::from_space(&state, &space).await?;
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

// ---------------------------------------------------------------------------
// Who can take part

#[derive(Template)]
#[template(path = "space_audience.html")]
struct AudienceTemplate {
    layout: Layout,
    audience: SpaceAudience,
}

impl AudienceTemplate {
    fn heading(&self, verb: &str) -> &'static str {
        match verb {
            "read" => "Who can read",
            "reply" => "Who can reply",
            "start" => "Who can start threads",
            _ => "Who runs it",
        }
    }

    /// What a grant naming nobody says.
    fn level_text(&self, grant: &GrantAudience) -> &'static str {
        match grant.level {
            Level::Public => "Anyone, including visitors who haven’t logged in.",
            Level::Members => "Every member.",
            Level::Staff => "The moderators and admins.",
            Level::Listed if grant.verb == "administer" => {
                "Nobody is named, so the moderators and admins run it until someone is."
            }
            Level::Listed => "Nobody.",
        }
    }

    /// A site group as "Organisers group", a list as it is named to the
    /// viewer, with a capital.
    fn group_heading(&self, group: &NamedGroup) -> String {
        if group.list {
            format::capitalised(&group.label)
        } else {
            format::group_name(&group.label)
        }
    }

    /// Whether to say that staff run every space as well: not where the
    /// grant already says it is them.
    fn staff_also(&self, grant: &GrantAudience) -> bool {
        match grant.level {
            Level::Staff => false,
            Level::Listed => !grant.groups.is_empty() || !grant.people.is_empty(),
            Level::Public | Level::Members => true,
        }
    }
}

/// `/c/{slug}/audience`: who may read, reply, start threads in and run a
/// space, for logged-in members who can read it (DESIGN.md §3.1, §3.3).
/// What it says depends on who is looking, so it is never cached.
pub async fn audience_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    if !well_formed_slug(&slug) {
        return web::not_found(&state, &viewer).await;
    }
    if viewer.user().is_none() {
        return Ok(web::login_first(&format!("/c/{slug}/audience")));
    }
    let Some(audience) = content::space_audience(state.db(), &viewer, &slug).await? else {
        return web::not_found(&state, &viewer).await;
    };
    render(
        StatusCode::OK,
        &AudienceTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            audience,
        },
    )
}
