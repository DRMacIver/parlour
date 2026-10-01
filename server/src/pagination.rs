//! "Go to page" links: the first and last pages, and the pages near the
//! current one, with gaps between.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageItem {
    Link(i64),
    Current(i64),
    Gap,
}

pub struct Pagination {
    /// The URL of page 1; other pages add `?page=n`.
    base: String,
    pub current: i64,
    pub total: i64,
}

/// How many pages either side of the current one get their own link.
const NEAR: i64 = 2;

impl Pagination {
    pub fn new(base: impl Into<String>, current: i64, total: i64) -> Pagination {
        Pagination {
            base: base.into(),
            current,
            total,
        }
    }

    pub fn href(&self, page: &i64) -> String {
        page_href(&self.base, *page)
    }

    pub fn is_multi_page(&self) -> bool {
        self.total > 1
    }

    pub fn prev(&self) -> Option<i64> {
        (self.current > 1).then(|| self.current - 1)
    }

    pub fn next(&self) -> Option<i64> {
        (self.current < self.total).then(|| self.current + 1)
    }

    pub fn items(&self) -> Vec<PageItem> {
        let wanted =
            |page: i64| page == 1 || page == self.total || (page - self.current).abs() <= NEAR;
        let mut items = Vec::new();
        let mut previous = 0;
        for page in 1..=self.total {
            if !wanted(page) {
                continue;
            }
            if page > previous + 1 {
                items.push(PageItem::Gap);
            }
            items.push(if page == self.current {
                PageItem::Current(page)
            } else {
                PageItem::Link(page)
            });
            previous = page;
        }
        items
    }
}

pub fn page_href(base: &str, page: i64) -> String {
    if page <= 1 {
        base.to_owned()
    } else {
        format!("{base}?page={page}")
    }
}

/// The compact page links shown under a thread's title in thread lists:
/// "1 2 3 … 9".
pub fn thread_list_pages(total: i64) -> Vec<PageItem> {
    if total <= 1 {
        return Vec::new();
    }
    if total <= 4 {
        return (1..=total).map(PageItem::Link).collect();
    }
    vec![
        PageItem::Link(1),
        PageItem::Link(2),
        PageItem::Link(3),
        PageItem::Gap,
        PageItem::Link(total),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    fn pages(items: &[PageItem]) -> Vec<i64> {
        items
            .iter()
            .filter_map(|item| match item {
                PageItem::Link(p) | PageItem::Current(p) => Some(*p),
                PageItem::Gap => None,
            })
            .collect()
    }

    #[test]
    fn examples() {
        use PageItem::*;
        assert_eq!(Pagination::new("/x", 1, 1).items(), vec![Current(1)]);
        assert_eq!(
            Pagination::new("/x", 1, 15).items(),
            vec![Current(1), Link(2), Link(3), Gap, Link(15)]
        );
        assert_eq!(
            Pagination::new("/x", 8, 15).items(),
            vec![
                Link(1),
                Gap,
                Link(6),
                Link(7),
                Current(8),
                Link(9),
                Link(10),
                Gap,
                Link(15)
            ]
        );
        assert_eq!(Pagination::new("/x", 2, 3).href(&1), "/x");
        assert_eq!(Pagination::new("/x", 2, 3).href(&3), "/x?page=3");
    }

    #[hegel::test(test_cases = 500)]
    fn page_links_are_well_formed(tc: TestCase) {
        let total = tc.draw(gs::integers::<i64>().min_value(1).max_value(10_000));
        let current = tc.draw(gs::integers::<i64>().min_value(1).max_value(total));
        let items = Pagination::new("/x", current, total).items();
        let shown = pages(&items);
        // Strictly increasing, within range, always first, last and current.
        assert!(shown.windows(2).all(|w| w[0] < w[1]), "{items:?}");
        assert!(shown.iter().all(|p| (1..=total).contains(p)));
        assert_eq!(shown.first(), Some(&1));
        assert_eq!(shown.last(), Some(&total));
        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i, PageItem::Current(_)))
                .count(),
            1
        );
        assert!(items.contains(&PageItem::Current(current)));
        // A gap appears exactly where pages are skipped.
        for window in items.windows(3) {
            if let [
                PageItem::Link(a) | PageItem::Current(a),
                PageItem::Gap,
                PageItem::Link(b) | PageItem::Current(b),
            ] = window
            {
                assert!(b - a > 1, "{items:?}");
            }
        }
        for window in items.windows(2) {
            if let [
                PageItem::Link(a) | PageItem::Current(a),
                PageItem::Link(b) | PageItem::Current(b),
            ] = window
            {
                assert_eq!(b - a, 1, "{items:?}");
            }
        }
        // Never more than a handful of links.
        assert!(items.len() <= 9);
    }

    #[hegel::test]
    fn thread_list_pages_cover_first_and_last(tc: TestCase) {
        let total = tc.draw(gs::integers::<i64>().min_value(0).max_value(10_000));
        let items = thread_list_pages(total);
        let shown = pages(&items);
        if total <= 1 {
            assert!(items.is_empty());
        } else {
            assert_eq!(shown.first(), Some(&1));
            assert_eq!(shown.last(), Some(&total));
            assert!(shown.windows(2).all(|w| w[0] < w[1]));
        }
    }
}
