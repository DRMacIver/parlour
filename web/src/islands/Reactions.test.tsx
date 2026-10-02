import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import Reactions, { names } from "./Reactions";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const props = {
  post: 7,
  reactions: [{ emoji: "👍", count: 2, mine: false, names: ["Ruth", "Jonah"] }],
  types: ["👍", "❤️"],
  endpoint: "/api/posts/7/reactions",
  who: "/p/7/reactions",
};

describe("Reactions", () => {
  it("lists names as the server does", () => {
    expect(names([])).toBe("");
    expect(names(["Ruth"])).toBe("Ruth");
    expect(names(["Ruth", "Jonah", "Maya"])).toBe("Ruth, Jonah and Maya");
  });

  it("toggles a reaction and shows the counts the server sends back", async () => {
    const fetch = vi.fn(async () =>
      new Response(
        JSON.stringify({ reactions: [{ emoji: "👍", count: 3, mine: true, names: ["Ruth", "Jonah", "Ana"] }] }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      ),
    );
    vi.stubGlobal("fetch", fetch);
    render(<Reactions {...props} />);
    const button = screen.getByRole("button", { pressed: false, name: /reactions; add yours/ });
    expect(button.textContent).toContain("2");
    await act(async () => {
      fireEvent.click(button);
    });
    expect(fetch).toHaveBeenCalledWith(
      "/api/posts/7/reactions",
      expect.objectContaining({ method: "POST", body: JSON.stringify({ emoji: "👍", action: "add" }) }),
    );
    const pressed = screen.getByRole("button", { pressed: true });
    expect(pressed.textContent).toContain("3");
    expect(pressed.title).toBe("Ruth, Jonah and Ana");
  });

  it("says why when the server refuses", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ error: "You are silenced." }), { status: 403 })),
    );
    render(<Reactions {...props} />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "React with ❤️" }));
    });
    expect(screen.getByRole("alert").textContent).toBe("You are silenced.");
    expect(screen.getByRole("button", { pressed: false, name: /reactions/ }).textContent).toContain("2");
  });
});
