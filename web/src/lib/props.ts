// Islands read their props from a <script type="application/json"
// data-island-props> element straight after their own element.

export function readProps(element: Element): Record<string, unknown> {
  const next = element.nextElementSibling;
  if (
    next instanceof HTMLScriptElement &&
    next.type === "application/json" &&
    next.hasAttribute("data-island-props")
  ) {
    const parsed: unknown = JSON.parse(next.textContent ?? "{}");
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      return parsed as Record<string, unknown>;
    }
  }
  return {};
}
