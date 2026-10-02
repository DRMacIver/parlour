//! Word-level differences between two versions of a post, for the edit
//! history page (DESIGN.md §3.3).

use similar::{ChangeTag, TextDiff};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Same(String),
    Added(String),
    Removed(String),
}

/// The changes from `old` to `new`, word by word. Runs of the same kind
/// are joined, so no two neighbouring segments are of the same kind.
pub fn words(old: &str, new: &str) -> Vec<Segment> {
    let diff = TextDiff::from_words(old, new);
    let mut out: Vec<Segment> = Vec::new();
    for change in diff.iter_all_changes() {
        let text = change.value();
        match (change.tag(), out.last_mut()) {
            (ChangeTag::Equal, Some(Segment::Same(s)))
            | (ChangeTag::Insert, Some(Segment::Added(s)))
            | (ChangeTag::Delete, Some(Segment::Removed(s))) => s.push_str(text),
            (ChangeTag::Equal, _) => out.push(Segment::Same(text.to_owned())),
            (ChangeTag::Insert, _) => out.push(Segment::Added(text.to_owned())),
            (ChangeTag::Delete, _) => out.push(Segment::Removed(text.to_owned())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    fn old_side(segments: &[Segment]) -> String {
        segments
            .iter()
            .filter_map(|s| match s {
                Segment::Same(t) | Segment::Removed(t) => Some(t.as_str()),
                Segment::Added(_) => None,
            })
            .collect()
    }

    fn new_side(segments: &[Segment]) -> String {
        segments
            .iter()
            .filter_map(|s| match s {
                Segment::Same(t) | Segment::Added(t) => Some(t.as_str()),
                Segment::Removed(_) => None,
            })
            .collect()
    }

    #[test]
    fn shows_changed_words() {
        assert_eq!(
            words(
                "I think solo play is fun.",
                "I think solo play is great fun."
            ),
            vec![
                Segment::Same("I think solo play is ".into()),
                Segment::Added("great ".into()),
                Segment::Same("fun.".into()),
            ]
        );
        assert_eq!(words("same", "same"), vec![Segment::Same("same".into())]);
        assert_eq!(words("", ""), vec![]);
    }

    fn texts() -> impl Generator<String> {
        gs::vecs(gs::one_of(vec![
            gs::text().max_size(6).boxed(),
            gs::sampled_from(vec!["a", "b", " ", "\n", "  ", "word", "é"])
                .map(String::from)
                .boxed(),
        ]))
        .map(|parts| parts.concat())
    }

    /// Both versions can be read back out of the differences.
    #[hegel::test(test_cases = 500)]
    fn both_versions_are_recoverable(tc: TestCase) {
        let old = tc.draw_silent(texts());
        let new = tc.draw_silent(texts());
        tc.note(&format!("{old:?} -> {new:?}"));
        let segments = words(&old, &new);
        assert_eq!(old_side(&segments), old);
        assert_eq!(new_side(&segments), new);
        for pair in segments.windows(2) {
            assert_ne!(
                std::mem::discriminant(&pair[0]),
                std::mem::discriminant(&pair[1])
            );
        }
        for segment in &segments {
            assert!(
                !matches!(segment, Segment::Same(t) | Segment::Added(t) | Segment::Removed(t) if t.is_empty())
            );
        }
    }

    /// Comparing a version with itself shows no changes.
    #[hegel::test(test_cases = 300)]
    fn no_changes_between_equal_versions(tc: TestCase) {
        let text = tc.draw_silent(texts());
        for segment in words(&text, &text) {
            assert!(matches!(segment, Segment::Same(_)), "{text:?}");
        }
    }
}
