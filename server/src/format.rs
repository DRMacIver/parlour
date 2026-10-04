//! Small display helpers called from templates.
//!
//! Times are shown as absolute UTC dates rather than "today" or "5 minutes
//! ago": anonymous pages are cached until something they show changes, and a
//! relative time would go stale without anything changing. The footer of
//! every page says that times are UTC.

use std::borrow::Borrow;

use chrono::{DateTime, Utc};

/// "1 Oct 2026, 21:14"
pub fn datetime(at: &DateTime<Utc>) -> String {
    at.format("%-d %b %Y, %H:%M").to_string()
}

/// "1 Oct 2026"
pub fn date(at: &DateTime<Utc>) -> String {
    at.format("%-d %b %Y").to_string()
}

/// "March 2024"
pub fn month_year(at: &DateTime<Utc>) -> String {
    at.format("%B %Y").to_string()
}

/// For `<time datetime="…">`.
pub fn iso(at: &DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// "1,204"
/// Takes the number by value or reference, as templates pass either.
pub fn count(n: impl Borrow<i64>) -> String {
    let n = n.borrow();
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if *n < 0 {
        out.insert(0, '-');
    }
    out
}

/// "1 post", "2 posts"
pub fn plural(n: impl Borrow<i64>, one: &str, many: &str) -> String {
    let n = *n.borrow();
    format!("{} {}", count(n), if n == 1 { one } else { many })
}

/// The letter shown in a user's placeholder avatar.
pub fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}

/// One of a fixed set of avatar colour classes in parlour.css.
pub fn avatar_class(user_id: &i64) -> String {
    format!("avatar-{}", user_id.rem_euclid(8))
}

/// Whether `id` is the logged-in viewer's.
pub fn is_viewer(viewer_id: &Option<i64>, id: &i64) -> bool {
    *viewer_id == Some(*id)
}

/// The people who reacted, for a tooltip: up to 20 names, then a count of
/// the rest.
pub fn names(names: &[String]) -> String {
    const SHOWN: usize = 20;
    let mut text = names
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        text.push_str(&format!(" and {} more", names.len() - SHOWN));
    }
    text
}

/// Who can read a space, for a label beside its name: nothing for a
/// public space, "Members only", "Staff only", the site groups and lists
/// that may read it, or "Chosen members" when only people are named.
/// Lists come as the viewer may know them ("Ruth’s Friends (12)" or
/// "people Ruth has chosen", from `list_labels` in SQL).
pub fn access_label(
    read_level: &str,
    group_names: &[String],
    list_names: &[String],
) -> Option<String> {
    match read_level {
        "public" => None,
        "members" => Some("Members only".to_owned()),
        "staff" => Some("Staff only".to_owned()),
        _ => {
            let groups = match group_names {
                [] => None,
                [one] if without_group(one).len() < one.len() => Some(one.clone()),
                [one] => Some(format!("{one} group")),
                [first @ .., last] => Some(format!(
                    "{} and {} groups",
                    first
                        .iter()
                        .map(|name| without_group(name))
                        .collect::<Vec<_>>()
                        .join(", "),
                    without_group(last)
                )),
            };
            let parts: Vec<&str> = groups
                .as_deref()
                .into_iter()
                .chain(list_names.iter().map(String::as_str))
                .collect();
            Some(match parts.as_slice() {
                [] => "Chosen members".to_owned(),
                [one] => capitalised(one),
                [first @ .., last] => capitalised(&format!("{} and {last}", first.join(", "))),
            })
        }
    }
}

/// Text with its first letter in upper case.
fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Who a grant lets post or administer, in a word or two.
pub fn grant_label(level: crate::permissions::Level) -> &'static str {
    use crate::permissions::Level;
    match level {
        Level::Public => "Everyone",
        Level::Members => "Any member who can read it",
        Level::Staff => "Staff only",
        Level::Listed => "Chosen members and groups",
    }
}

/// Whether a space's access label names groups or lists (rather than
/// saying "Members only" or "Staff only"), for its icon.
pub fn access_is_groups(read_level: &str, group_names: &[String], list_names: &[String]) -> bool {
    read_level == "listed" && !(group_names.is_empty() && list_names.is_empty())
}

/// A group's name without a final word "group", which the label adds.
fn without_group(name: &str) -> &str {
    const SUFFIX: &str = " group";
    match name.len().checked_sub(SUFFIX.len()) {
        Some(cut)
            if cut > 0
                && name
                    .get(cut..)
                    .is_some_and(|end| end.eq_ignore_ascii_case(SUFFIX)) =>
        {
            &name[..cut]
        }
        _ => name,
    }
}

pub fn thread_href(id: &i64, slug: &str) -> String {
    format!("/t/{id}/{slug}")
}

/// A link to page `page` of a thread.
pub fn thread_page_href(id: &i64, slug: &str, page: &i64) -> String {
    if *page <= 1 {
        thread_href(id, slug)
    } else {
        format!("/t/{id}/{slug}?page={page}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn formats_dates() {
        let at = Utc.with_ymd_and_hms(2026, 10, 1, 9, 5, 0).unwrap();
        assert_eq!(datetime(&at), "1 Oct 2026, 09:05");
        assert_eq!(date(&at), "1 Oct 2026");
        assert_eq!(month_year(&at), "October 2026");
        assert_eq!(iso(&at), "2026-10-01T09:05:00Z");
    }

    #[test]
    fn labels_who_can_read() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(access_label("public", &[], &[]), None);
        assert_eq!(access_label("members", &[], &[]).unwrap(), "Members only");
        assert_eq!(access_label("staff", &[], &[]).unwrap(), "Staff only");
        assert_eq!(access_label("listed", &[], &[]).unwrap(), "Chosen members");
        assert_eq!(
            access_label("listed", &names(&["Organisers"]), &[]).unwrap(),
            "Organisers group"
        );
        assert_eq!(
            access_label(
                "listed",
                &names(&["Board", "Organisers", "Treasurers"]),
                &[]
            )
            .unwrap(),
            "Board, Organisers and Treasurers groups"
        );
        // A name that already says "group" doesn't say it twice.
        assert_eq!(
            access_label("listed", &names(&["Organisers group"]), &[]).unwrap(),
            "Organisers group"
        );
        assert_eq!(
            access_label("listed", &names(&["Steering Group"]), &[]).unwrap(),
            "Steering Group"
        );
        assert_eq!(
            access_label("listed", &names(&["Board group", "Treasurers"]), &[]).unwrap(),
            "Board and Treasurers groups"
        );
        assert_eq!(
            access_label("listed", &names(&["Playgroup"]), &[]).unwrap(),
            "Playgroup group"
        );
        assert_eq!(
            access_label("listed", &names(&["Group"]), &[]).unwrap(),
            "Group group"
        );
        // Lists come as the viewer may know them (DESIGN.md §3.3).
        assert_eq!(
            access_label("listed", &[], &names(&["Ruth’s Friends (12)"])).unwrap(),
            "Ruth’s Friends (12)"
        );
        assert_eq!(
            access_label("listed", &[], &names(&["people Ruth has chosen"])).unwrap(),
            "People Ruth has chosen"
        );
        assert_eq!(
            access_label(
                "listed",
                &names(&["Organisers"]),
                &names(&["Ruth’s Friends (2)", "people Kit has chosen"])
            )
            .unwrap(),
            "Organisers group, Ruth’s Friends (2) and people Kit has chosen"
        );
        assert!(access_is_groups(
            "listed",
            &[],
            &names(&["people Kit has chosen"])
        ));
        assert!(!access_is_groups("listed", &[], &[]));
        assert!(!access_is_groups("members", &[], &[]));
    }

    #[test]
    fn formats_counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(1204), "1,204");
        assert_eq!(count(-1234567), "-1,234,567");
        assert_eq!(plural(1, "post", "posts"), "1 post");
        assert_eq!(plural(2, "post", "posts"), "2 posts");
    }

    #[hegel::test]
    fn count_round_trips(tc: TestCase) {
        let n = tc.draw(gs::integers::<i64>());
        let shown = count(n);
        assert_eq!(shown.replace(',', "").parse::<i64>().unwrap(), n);
        for group in shown.trim_start_matches('-').split(',').skip(1) {
            assert_eq!(group.len(), 3, "{shown}");
        }
    }

    #[test]
    fn initials() {
        assert_eq!(initial("ruth"), "R");
        assert_eq!(initial("_dev"), "D");
        assert_eq!(initial("ünal"), "Ü");
        assert_eq!(initial(""), "?");
    }

    #[hegel::test]
    fn avatar_classes_are_in_range(tc: TestCase) {
        let id = tc.draw(gs::integers::<i64>());
        let class = avatar_class(&id);
        let n: i64 = class.strip_prefix("avatar-").unwrap().parse().unwrap();
        assert!((0..8).contains(&n));
    }
}
