//! Markdown to HTML for posts (DESIGN.md §3.3): CommonMark plus the GitHub
//! extensions, rendered by comrak and then sanitised by ammonia. The HTML is
//! generated when a post is saved and stored as `posts.body_html`.
//!
//! Mentions (`@username`) of members who exist when the post is saved
//! become links to their profiles. Text in code, links and images is left
//! alone.

use std::cell::RefCell;
use std::sync::LazyLock;

use comrak::arena_tree::Node;
use comrak::nodes::{Ast, AstNode, LineColumn, NodeLink, NodeValue};

/// A rendered post.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// Sanitised HTML, safe to include in a page as-is.
    pub html: String,
    /// The members mentioned, by their canonical usernames, each once, in
    /// the order first mentioned.
    pub mentions: Vec<String>,
}

fn options() -> comrak::Options<'static> {
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options
}

/// Renders a post body, with no mentions resolved.
pub fn render(source: &str) -> String {
    render_with_mentions(source, |_| None).html
}

/// The usernames that `@` mentions in `source` might refer to, lowercased
/// and each once. Look these up, then pass the ones that exist to
/// [`render_with_mentions`].
pub fn mention_candidates(source: &str) -> Vec<String> {
    let arena = comrak::Arena::new();
    let root = comrak::parse_document(&arena, source, &options());
    let mut names = Vec::new();
    for node in mentionable_text_nodes(root) {
        if let NodeValue::Text(text) = &node.data.borrow().value {
            for (_, name) in find_mentions(text) {
                let name = name.to_ascii_lowercase();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names
}

/// Renders a post body. `resolve` gives the canonical username for a
/// mentioned name (as typed) if that member exists.
pub fn render_with_mentions(source: &str, resolve: impl Fn(&str) -> Option<String>) -> Rendered {
    let options = options();
    let arena = comrak::Arena::new();
    let root = comrak::parse_document(&arena, source, &options);
    let mut mentions: Vec<String> = Vec::new();
    for node in mentionable_text_nodes(root) {
        let text = match &node.data.borrow().value {
            NodeValue::Text(text) => text.to_string(),
            _ => continue,
        };
        let found: Vec<(usize, String, String)> = find_mentions(&text)
            .into_iter()
            .filter_map(|(at, name)| Some((at, name.to_owned(), resolve(name)?)))
            .collect();
        if found.is_empty() {
            continue;
        }
        let mut rest = 0;
        for (at, typed, username) in found {
            if at > rest {
                node.insert_before(new_node(
                    &arena,
                    NodeValue::Text(text[rest..at].to_owned().into()),
                ));
            }
            let link = new_node(
                &arena,
                NodeValue::Link(Box::new(NodeLink {
                    url: format!("/u/{username}"),
                    title: String::new(),
                })),
            );
            link.append(new_node(
                &arena,
                NodeValue::Text(format!("@{typed}").into()),
            ));
            node.insert_before(link);
            rest = at + 1 + typed.len();
            if !mentions.contains(&username) {
                mentions.push(username);
            }
        }
        if rest < text.len() {
            node.insert_before(new_node(
                &arena,
                NodeValue::Text(text[rest..].to_owned().into()),
            ));
        }
        node.detach();
    }
    let mut html = String::new();
    comrak::format_html(root, &options, &mut html).expect("writing to a string can't fail");
    Rendered {
        html: SANITIZER.clean(&html).to_string(),
        mentions,
    }
}

fn new_node<'a>(arena: &'a comrak::Arena<'a>, value: NodeValue) -> &'a AstNode<'a> {
    arena.alloc(Node::new(RefCell::new(Ast::new(
        value,
        LineColumn { line: 0, column: 0 },
    ))))
}

/// Text nodes outside links and images, with adjacent text merged so that
/// a mention split across nodes by the parser is still found.
fn mentionable_text_nodes<'a>(root: &'a AstNode<'a>) -> Vec<&'a AstNode<'a>> {
    // Merge runs of adjacent text nodes.
    let parents: Vec<&AstNode> = root.descendants().collect();
    for parent in parents {
        let mut child = parent.first_child();
        while let Some(node) = child {
            let next = node.next_sibling();
            if let Some(next_node) = next {
                let merged = {
                    let mut first = node.data.borrow_mut();
                    let second = next_node.data.borrow();
                    match (&mut first.value, &second.value) {
                        (NodeValue::Text(a), NodeValue::Text(b)) => {
                            let joined = format!("{a}{b}");
                            *a = joined.into();
                            true
                        }
                        _ => false,
                    }
                };
                if merged {
                    next_node.detach();
                    continue;
                }
            }
            child = next;
        }
    }
    root.descendants()
        .filter(|node| matches!(node.data.borrow().value, NodeValue::Text(_)))
        .filter(|node| {
            !node.ancestors().skip(1).any(|a| {
                matches!(
                    a.data.borrow().value,
                    NodeValue::Link(_) | NodeValue::Image(_)
                )
            })
        })
        .collect()
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Each `@name` in plain text, as (byte offset of the `@`, name). A name
/// has the form usernames have (2 to 24 letters, digits and underscores),
/// and the `@` must not follow a letter, digit, `_`, `@` or `.`, so email
/// addresses aren't taken for mentions.
pub fn find_mentions(text: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut previous: Option<char> = None;
    for (at, c) in text.char_indices() {
        if c == '@' && !previous.is_some_and(|p| is_name_char(p) || p == '@' || p == '.') {
            let rest = &text[at + 1..];
            let len = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
            if (2..=24).contains(&len) {
                found.push((at, &rest[..len]));
            }
        }
        previous = Some(c);
    }
    found
}

static SANITIZER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(|| {
    let mut builder = ammonia::Builder::default();
    builder
        .link_rel(Some("nofollow ugc noopener"))
        .add_tags(["input"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_tag_attributes("ol", ["start"]);
    builder
});

#[cfg(test)]
mod tests {
    use super::{find_mentions, mention_candidates, render, render_with_mentions};
    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator, PrintableGenerator};

    #[test]
    fn renders_commonmark() {
        assert_eq!(
            render("Hello *world*\n\n> quoted"),
            "<p>Hello <em>world</em></p>\n<blockquote>\n<p>quoted</p>\n</blockquote>\n"
        );
    }

    #[test]
    fn renders_github_extensions() {
        let html = render("| a | b |\n|---|---|\n| 1 | 2 |\n\n~~gone~~ https://example.com");
        assert!(html.contains("<table>"), "{html}");
        assert!(html.contains("<del>gone</del>"), "{html}");
        assert!(html.contains("<a href=\"https://example.com\""), "{html}");
    }

    #[test]
    fn links_are_marked_as_user_content() {
        let html = render("[x](https://example.com)");
        assert!(html.contains("rel=\"nofollow ugc noopener\""), "{html}");
    }

    #[test]
    fn raw_html_is_removed() {
        let html = render("<script>alert(1)</script><img src=x onerror=alert(1)>");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("onerror"), "{html}");
    }

    #[test]
    fn javascript_links_are_removed() {
        let html = render("[click](javascript:alert(1))");
        assert!(!html.contains("javascript:"), "{html}");
    }

    /// The start tags in some HTML, as (name, [(attribute, value)]).
    fn start_tags(html: &str) -> Vec<(String, Vec<(String, String)>)> {
        let mut tags = Vec::new();
        let mut rest = html;
        while let Some(start) = rest.find('<') {
            rest = &rest[start + 1..];
            if rest.starts_with('/') || rest.starts_with('!') {
                continue;
            }
            let name_len = rest
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(rest.len());
            let name = rest[..name_len].to_ascii_lowercase();
            rest = &rest[name_len..];
            let mut attrs = Vec::new();
            loop {
                rest = rest.trim_start();
                if rest.is_empty() || rest.starts_with('>') || rest.starts_with("/>") {
                    break;
                }
                let attr_len = rest
                    .find(|c: char| c == '=' || c == '>' || c.is_whitespace())
                    .unwrap_or(rest.len());
                let attr = rest[..attr_len].to_ascii_lowercase();
                rest = &rest[attr_len..];
                let mut value = String::new();
                if let Some(after) = rest.strip_prefix("=\"") {
                    let end = after.find('"').expect("unterminated attribute");
                    value = after[..end].to_owned();
                    rest = &after[end + 1..];
                }
                attrs.push((attr, value));
            }
            tags.push((name, attrs));
        }
        tags
    }

    /// Whatever the input, the output contains no dangerous elements, no
    /// event handler attributes and no javascript: URLs.
    #[hegel::test(test_cases = 500)]
    fn output_is_always_sanitised(tc: TestCase) {
        let pieces = tc.draw(gs::vecs(gs::one_of(vec![
            gs::text().boxed_printable(),
            gs::sampled_from(vec![
                "<script>",
                "</script>",
                "<img src=x onerror=alert(1)>",
                "[a](javascript:alert(1))",
                "<a href=\"javascript:x\">",
                "`",
                "```",
                "\n",
                "> ",
                "* ",
                "|",
                "---",
                "<",
                ">",
                "\"",
                "<svg onload=alert(1)>",
                "<iframe src=//evil>",
                "<style>",
                "&lt;script&gt;",
                "![i](x)",
                "[l]: javascript:x\n",
                "[l]",
                "<https://a.b>",
                "- [ ] ",
                "&#x6a;avascript:",
                "@ruth",
                "@",
                "@<script>",
                "[@ruth](javascript:x)",
            ])
            .map(String::from)
            .boxed_printable(),
        ])));
        let source: String = pieces.concat();
        // Every name mentioned resolves, so that links are made too.
        let html = render_with_mentions(&source, |name| Some(name.to_owned())).html;
        for (name, attrs) in start_tags(&html) {
            assert!(
                ![
                    "script", "iframe", "svg", "style", "object", "embed", "form"
                ]
                .contains(&name.as_str()),
                "<{name}> in {html}"
            );
            for (attr, value) in attrs {
                assert!(!attr.starts_with("on"), "{attr} on <{name}> in {html}");
                let value = value.to_ascii_lowercase().replace(['\t', '\n', ' '], "");
                assert!(
                    !value.starts_with("javascript:"),
                    "{attr}={value} in {html}"
                );
            }
        }
    }

    fn known(name: &str) -> Option<String> {
        ["ruth", "Jonah_2"]
            .into_iter()
            .find(|known| known.eq_ignore_ascii_case(name))
            .map(str::to_owned)
    }

    #[test]
    fn mentions_of_members_become_links() {
        let rendered = render_with_mentions("Thanks @ruth and @JONAH_2, not @nobody.", known);
        assert_eq!(
            rendered.html,
            "<p>Thanks <a href=\"/u/ruth\" rel=\"nofollow ugc noopener\">@ruth</a> and \
             <a href=\"/u/Jonah_2\" rel=\"nofollow ugc noopener\">@JONAH_2</a>, not @nobody.</p>\n"
        );
        assert_eq!(rendered.mentions, vec!["ruth", "Jonah_2"]);
    }

    #[test]
    fn mentions_in_code_links_and_emails_are_left_alone() {
        for source in [
            "`@ruth`",
            "```\n@ruth\n```",
            "    @ruth",
            "[@ruth](https://example.com)",
            "write to ruth@ruth.example",
            "me@ruth",
            "@@ruth",
            "@ruthruthruthruthruthruthr",
        ] {
            let rendered = render_with_mentions(source, known);
            assert!(rendered.mentions.is_empty(), "{source}: {rendered:?}");
            assert!(!rendered.html.contains("/u/ruth"), "{source}: {rendered:?}");
        }
    }

    #[test]
    fn mentions_in_emphasis_quotes_and_lists_are_found() {
        let rendered = render_with_mentions("> **@ruth** wrote:\n\n- hi @ruth!", known);
        assert_eq!(rendered.mentions, vec!["ruth"]);
        assert_eq!(
            rendered.html.matches("href=\"/u/ruth\"").count(),
            2,
            "{}",
            rendered.html
        );
    }

    #[test]
    fn candidates_are_the_names_mentioned_outside_code() {
        assert_eq!(
            mention_candidates("@Ruth, `@kit` and @ruth or @Dev_2."),
            vec!["ruth", "dev_2"]
        );
    }

    /// Resolving no names renders exactly what plain rendering does, and
    /// the text of a post is the same whether or not names resolve.
    #[hegel::test(test_cases = 300)]
    fn mentions_only_add_links(tc: TestCase) {
        let pieces = tc.draw(gs::vecs(gs::one_of(vec![
            gs::text().max_size(10).boxed_printable(),
            gs::sampled_from(vec![
                "@ruth", "@jonah_2", "@x", " ", "\n", "`", "*", "[", "](/a)", "> ", "@", ".",
            ])
            .map(String::from)
            .boxed_printable(),
        ])));
        let source: String = pieces.concat();
        let plain = render(&source);
        assert_eq!(render_with_mentions(&source, |_| None).html, plain);
        let linked = render_with_mentions(&source, known);
        let strip = |html: &str| {
            let mut out = String::new();
            let mut in_tag = false;
            for c in html.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    c if !in_tag => out.push(c),
                    _ => {}
                }
            }
            out
        };
        assert_eq!(strip(&linked.html), strip(&plain), "{source:?}");
        for name in &linked.mentions {
            assert!(known(name).is_some());
        }
    }

    /// Every mention found is an `@` followed by a whole name.
    #[hegel::test(test_cases = 500)]
    fn found_mentions_are_whole_names(tc: TestCase) {
        let text = tc.draw(gs::text());
        for (at, name) in find_mentions(&text) {
            assert_eq!(&text[at..at + 1], "@");
            assert_eq!(&text[at + 1..at + 1 + name.len()], name);
            assert!((2..=24).contains(&name.len()));
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
            let after = text[at + 1 + name.len()..].chars().next();
            assert!(!after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_'));
        }
    }
}
