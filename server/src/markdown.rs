//! Markdown to HTML for posts (DESIGN.md §3.3): CommonMark plus the GitHub
//! extensions, rendered by comrak and then sanitised by ammonia. The HTML is
//! generated when a post is saved and stored as `posts.body_html`.

use std::sync::LazyLock;

/// Renders a post body. The result is safe to include in a page as-is.
pub fn render(source: &str) -> String {
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    let html = comrak::markdown_to_html(source, &options);
    SANITIZER.clean(&html).to_string()
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
    use super::render;
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
            ])
            .map(String::from)
            .boxed_printable(),
        ])));
        let source: String = pieces.concat();
        let html = render(&source);
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
}
