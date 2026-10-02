// Mounts each island on its element. The server renders a plain HTML form
// (or link) inside every element marked with data-island; the island
// replaces it, reading its props from the JSON script tag that follows the
// element. Islands load only on pages for logged-in members.

import { createRoot } from "react-dom/client";
import type { ComponentType } from "react";
import { readProps } from "./lib/props";

type Loader = () => Promise<{ default: ComponentType<never> }>;

const islands: Record<string, Loader> = {
  composer: () => import("./islands/Composer"),
  reactions: () => import("./islands/Reactions"),
  notifications: () => import("./islands/Notifications"),
  "new-posts": () => import("./islands/NewPosts"),
  "fork-dialog": () => import("./islands/ForkDialog"),
};

export async function mount(element: HTMLElement): Promise<void> {
  const name = element.dataset.island ?? "";
  const load = islands[name];
  if (!load || element.dataset.islandMounted) {
    return;
  }
  element.dataset.islandMounted = "true";
  const props = readProps(element);
  const { default: Island } = await load();
  const Component = Island as ComponentType<Record<string, unknown>>;
  createRoot(element).render(<Component {...props} element={element} />);
}

for (const element of document.querySelectorAll<HTMLElement>("[data-island]")) {
  mount(element).catch((error: unknown) => {
    // The plain form is still there and still works.
    console.error(`island ${element.dataset.island} failed to load`, error);
    delete element.dataset.islandMounted;
  });
}
