import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import LayoutEditor, { indexBefore, reordered, type Arrangement } from "./LayoutEditor";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const space = (id: number, slug: string) => ({
  id,
  slug,
  name: slug[0].toUpperCase() + slug.slice(1),
  essential: false,
  archived: false,
});

const layout: Arrangement = {
  sections: [
    { id: 1, name: "Forum", spaces: [space(10, "general"), space(11, "games")] },
    { id: 2, name: "Feeds", spaces: [] },
  ],
  available: [space(12, "films")],
};

describe("LayoutEditor", () => {
  it("counts a drop's index without the space being moved", () => {
    const spaces = [space(1, "a"), space(2, "b"), space(3, "c")];
    expect(indexBefore(spaces, 1, 3)).toBe(1);
    expect(indexBefore(spaces, 3, 1)).toBe(0);
    expect(indexBefore(spaces, 9, 2)).toBe(1);
  });

  it("moves a section one place, and not past either end", () => {
    expect(reordered(layout.sections, 2, -1)).toEqual([2, 1]);
    expect(reordered(layout.sections, 1, -1)).toEqual([1, 2]);
    expect(reordered(layout.sections, 2, 1)).toEqual([1, 2]);
  });

  it("saves a move and shows the layout the server sends back", async () => {
    const after: Arrangement = {
      sections: [
        { id: 1, name: "Forum", spaces: [space(10, "general"), space(11, "games")] },
        { id: 2, name: "Feeds", spaces: [space(12, "films")] },
      ],
      available: [],
    };
    const fetch = vi.fn(
      async () =>
        new Response(JSON.stringify({ layout: after }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
    );
    vi.stubGlobal("fetch", fetch);
    render(<LayoutEditor endpoint="/api/layout" admin={false} layout={layout} />);
    expect(screen.getByText("Not on your front page")).toBeTruthy();
    await act(async () => {
      fireEvent.change(screen.getByLabelText("Move Films to"), { target: { value: "2" } });
    });
    expect(fetch).toHaveBeenCalledWith(
      "/api/layout/spaces/12/move",
      expect.objectContaining({ method: "POST", body: JSON.stringify({ section: 2 }) }),
    );
    expect(screen.getByText("Every space you can read is in a section.")).toBeTruthy();
  });

  it("says what went wrong, keeping the layout as it was", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify({ error: "A section name should be between 1 and 80 characters." }), {
            status: 400,
            headers: { "Content-Type": "application/json" },
          }),
      ),
    );
    render(<LayoutEditor endpoint="/api/layout" admin={false} layout={layout} />);
    await act(async () => {
      fireEvent.change(screen.getByLabelText("Name"), { target: { value: "x" } });
      fireEvent.submit(screen.getByLabelText("Name").closest("form")!);
    });
    expect(screen.getByRole("alert").textContent).toContain("between 1 and 80");
    expect(screen.getAllByRole("region").length).toBeGreaterThan(0);
  });
});
