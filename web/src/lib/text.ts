// Editing a post's text.

/**
 * Inserts `addition` into `text` at `at`, on lines of its own when it is a
 * block (it ends with a newline) and the text around it isn't already
 * separated. Gives the new text and where the cursor goes.
 */
export function insertAt(text: string, at: number, addition: string): { text: string; cursor: number } {
  const position = Math.max(0, Math.min(at, text.length));
  const before = text.slice(0, position);
  const after = text.slice(position);
  let prefix = "";
  if (addition.endsWith("\n") && before.length > 0 && !before.endsWith("\n\n")) {
    prefix = before.endsWith("\n") ? "\n" : "\n\n";
  }
  const inserted = prefix + addition;
  return { text: before + inserted + after, cursor: before.length + inserted.length };
}

/** Turns text into Markdown blockquote lines. */
export function blockquote(text: string): string {
  return text
    .replace(/\r\n?/g, "\n")
    .trim()
    .split("\n")
    .map((line) => (line.trim() === "" ? ">" : `> ${line}`))
    .join("\n");
}

/**
 * The text selected inside the body of the post a link belongs to, as a
 * blockquote, or "" if nothing in that post is selected.
 */
export function quoteSelection(link: Element): string {
  const post = link.closest("article");
  const body = post?.querySelector(".post-body");
  const selection = window.getSelection();
  if (!body || !selection || selection.isCollapsed || selection.rangeCount === 0) {
    return "";
  }
  const range = selection.getRangeAt(0);
  if (!body.contains(range.commonAncestorContainer)) {
    return "";
  }
  const text = selection.toString();
  return text.trim() ? blockquote(text) : "";
}
