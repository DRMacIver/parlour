//! A layout as its editor shows it (DESIGN.md §3.5): the sections in
//! order, each with the spaces in it that the viewer can read, the spaces
//! they could add, and (for a member's own layout) the spaces they have
//! muted. Spaces the viewer can't read are left out of everything here,
//! but stay where they are in the layout: the editor's writes place a
//! space relative to the ones the viewer can see.

use serde::Serialize;

use crate::db::Db;
use crate::layouts;
use crate::viewer::Viewer;

use super::readable_spaces;

/// A space as the editor shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArrangedSpace {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub essential: bool,
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArrangedSection {
    pub id: i64,
    pub name: String,
    pub spaces: Vec<ArrangedSpace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Arrangement {
    #[serde(skip)]
    pub layout_id: i64,
    /// Whether this is the default layout.
    #[serde(skip)]
    pub default: bool,
    /// Whether the member has arranged it themselves (always false for the
    /// default).
    #[serde(skip)]
    pub edited: bool,
    pub sections: Vec<ArrangedSection>,
    /// Spaces in no section that could be put in one: readable, not
    /// archived, and not muted by the layout's owner. By name.
    pub available: Vec<ArrangedSpace>,
    /// The readable spaces the layout's owner has muted, by name.
    #[serde(skip)]
    pub muted: Vec<ArrangedSpace>,
}

impl Arrangement {
    /// The section a space is in, if the viewer can see it there.
    pub fn section_of(&self, space_id: i64) -> Option<&ArrangedSection> {
        self.sections
            .iter()
            .find(|s| s.spaces.iter().any(|c| c.id == space_id))
    }

    pub fn section(&self, section_id: i64) -> Option<&ArrangedSection> {
        self.sections.iter().find(|s| s.id == section_id)
    }

    /// Whether the viewer can see this space in the layout or add it.
    pub fn shows(&self, space_id: i64) -> bool {
        self.section_of(space_id).is_some()
            || self.available.iter().any(|c| c.id == space_id)
            || self.muted.iter().any(|c| c.id == space_id)
    }
}

/// The layout as the viewer may see it, or None if there is no such
/// layout. A member's mutes come from the layout's owner.
pub async fn arrangement(
    db: &Db,
    viewer: &Viewer,
    layout_id: i64,
) -> sqlx::Result<Option<Arrangement>> {
    let Some(layout) = layouts::read(db, layout_id).await? else {
        return Ok(None);
    };
    let readable = readable_spaces(db, viewer).await?;
    let muted_ids = match layout.user_id {
        Some(user_id) => layouts::muted_space_ids(db, user_id).await?,
        None => Vec::new(),
    };
    let arranged = |id: i64| {
        readable
            .iter()
            .find(|c| c.space.id == id)
            .map(|c| ArrangedSpace {
                id: c.space.id,
                slug: c.space.slug.clone(),
                name: c.space.name.clone(),
                essential: c.essential,
                archived: c.archived(),
            })
    };
    let placed: std::collections::HashSet<i64> = layout
        .sections
        .iter()
        .flat_map(|s| s.spaces.iter().map(|c| c.id))
        .collect();
    let sections = layout
        .sections
        .iter()
        .map(|s| ArrangedSection {
            id: s.id,
            name: s.name.clone(),
            spaces: s.spaces.iter().filter_map(|c| arranged(c.id)).collect(),
        })
        .collect();
    // `readable` is in order of name already.
    let available = readable
        .iter()
        .filter(|c| {
            !placed.contains(&c.space.id) && !c.archived() && !muted_ids.contains(&c.space.id)
        })
        .filter_map(|c| arranged(c.space.id))
        .collect();
    let muted = readable
        .iter()
        .filter(|c| muted_ids.contains(&c.space.id))
        .filter_map(|c| arranged(c.space.id))
        .collect();
    Ok(Some(Arrangement {
        layout_id: layout.id,
        default: layout.user_id.is_none(),
        edited: layout.edited_at.is_some(),
        sections,
        available,
        muted,
    }))
}
