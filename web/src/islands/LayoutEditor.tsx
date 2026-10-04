// The layout editor (DESIGN.md §3.5, §5): arranging a front page's
// sections and the spaces in them without a reload for each change.
// Spaces are dragged between sections, within a section, and off the front
// page; the buttons and "Move to" choices beside each space do the same
// for keyboards and touch screens. It replaces the plain forms (add,
// remove, move up, move down) that do all this a page at a time. Each
// change is saved at once, and the server answers with the layout as it
// now is, which is what is shown.

import { useState, type DragEvent, type FormEvent } from "react";
import { ApiError, postJson } from "../lib/api";

export interface Space {
  id: number;
  slug: string;
  name: string;
  essential: boolean;
  archived: boolean;
}

export interface Section {
  id: number;
  name: string;
  spaces: Space[];
}

export interface Arrangement {
  sections: Section[];
  available: Space[];
}

export interface LayoutEditorProps {
  /** `/api/layout` or `/api/admin/layout`. */
  endpoint: string;
  /** Whether this is the default front page, which admins edit. */
  admin: boolean;
  layout: Arrangement;
}

/** Where a dragged space would land. */
type Drop = { section: number; before: number | null } | "off" | null;

/** The index to ask for so that `moving` goes just before `before` in a
 * section, counting the spaces shown there without the one being moved. */
export function indexBefore(spaces: Space[], moving: number, before: number): number {
  return spaces.filter((c) => c.id !== moving).findIndex((c) => c.id === before);
}

/** The sections' ids with one moved by `step` places. */
export function reordered(sections: Section[], id: number, step: number): number[] {
  const ids = sections.map((s) => s.id);
  const at = ids.indexOf(id);
  const to = at + step;
  if (at < 0 || to < 0 || to >= ids.length) {
    return ids;
  }
  [ids[at], ids[to]] = [ids[to], ids[at]];
  return ids;
}

function Flags({ space }: { space: Space }) {
  return (
    <>
      {space.essential && <span className="flag">Essential</span>}
      {space.archived && <span className="flag">Archived</span>}
    </>
  );
}

function SectionName({
  section,
  busy,
  rename,
}: {
  section: Section;
  busy: boolean;
  rename: (name: string) => Promise<boolean>;
}) {
  const [name, setName] = useState(section.name);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!(await rename(name))) {
      setName(section.name);
    }
  };
  return (
    <form className="layout-section-form" onSubmit={submit}>
      <label className="visually-hidden" htmlFor={`section-name-${section.id}`}>
        Section name
      </label>
      <input
        id={`section-name-${section.id}`}
        type="text"
        maxLength={80}
        required
        value={name}
        onChange={(event) => setName(event.target.value)}
      />
      {name !== section.name && (
        <button className="button button-small button-quiet" type="submit" disabled={busy}>
          Rename
        </button>
      )}
    </form>
  );
}

export default function LayoutEditor(props: LayoutEditorProps) {
  const [layout, setLayout] = useState(props.layout);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState("");
  const [dragging, setDragging] = useState<number | null>(null);
  const [drop, setDrop] = useState<Drop>(null);
  const [confirming, setConfirming] = useState<number | null>(null);
  const [newName, setNewName] = useState("");

  /** Sends one change; true if it was made. */
  const send = async (path: string, body: unknown, done: string): Promise<boolean> => {
    setBusy(true);
    try {
      const result = await postJson<{ layout: Arrangement }>(`${props.endpoint}${path}`, body);
      setLayout(result.layout);
      setError(null);
      setStatus(done);
      return true;
    } catch (failure) {
      setError(failure instanceof ApiError ? failure.message : "That didn’t work. Try again.");
      setStatus("");
      return false;
    } finally {
      setBusy(false);
    }
  };

  const place = (space: Space, section: number, index: number | null) =>
    send(`/spaces/${space.id}/move`, index === null ? { section } : { section, index }, `Moved ${space.name}.`);
  const takeOff = (space: Space) =>
    send(`/spaces/${space.id}/remove`, {}, `${space.name} is off the front page.`);
  const order = (ids: number[]) => send("/sections/order", { sections: ids }, "Sections moved.");
  const essential = (space: Space) =>
    send(
      `/spaces/${space.id}/essential`,
      { essential: !space.essential },
      space.essential ? `${space.name} is no longer essential.` : `${space.name} is essential.`,
    );

  const find = (id: number): Space | undefined =>
    layout.sections.flatMap((s) => s.spaces).find((c) => c.id === id) ??
    layout.available.find((c) => c.id === id);

  // Dragging. What is dragged is remembered here rather than read from the
  // drag's data, which browsers hide until the drop.
  const start = (space: Space) => (event: DragEvent) => {
    event.dataTransfer.effectAllowed = "move";
    event.dataTransfer.setData("text/plain", String(space.id));
    setDragging(space.id);
  };
  const end = () => {
    setDragging(null);
    setDrop(null);
  };
  const over = (target: Drop) => (event: DragEvent) => {
    if (dragging === null) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    event.dataTransfer.dropEffect = "move";
    if (JSON.stringify(target) !== JSON.stringify(drop)) {
      setDrop(target);
    }
  };
  const dropped = (target: Drop) => async (event: DragEvent) => {
    event.preventDefault();
    event.stopPropagation();
    const space = dragging === null ? undefined : find(dragging);
    end();
    if (!space || target === null || busy) {
      return;
    }
    if (target === "off") {
      if (layout.available.some((c) => c.id === space.id)) {
        return;
      }
      await takeOff(space);
      return;
    }
    if (target.before === space.id) {
      return;
    }
    const section = layout.sections.find((s) => s.id === target.section);
    if (!section) {
      return;
    }
    const index = target.before === null ? null : indexBefore(section.spaces, space.id, target.before);
    await place(space, section.id, index);
  };

  const isDrop = (target: Drop) => JSON.stringify(target) === JSON.stringify(drop);

  /** The "Move to" choice beside a space. */
  const moveTo = (space: Space, current: number | null) => (
    <>
      <label className="visually-hidden" htmlFor={`move-${space.id}`}>
        Move {space.name} to
      </label>
      <select
        id={`move-${space.id}`}
        className="layout-move"
        value=""
        disabled={busy}
        onChange={(event) => {
          const value = event.target.value;
          if (value === "off") {
            void takeOff(space);
          } else if (value !== "") {
            void place(space, Number(value), null);
          }
        }}
      >
        <option value="">{current === null ? "Add to…" : "Move to…"}</option>
        {layout.sections
          .filter((s) => s.id !== current)
          .map((s) => (
            <option key={s.id} value={s.id}>
              {s.name}
            </option>
          ))}
        {current !== null && <option value="off">Off the front page</option>}
      </select>
    </>
  );

  const essentialButton = (space: Space) =>
    props.admin && (
      <button
        className="button button-small button-quiet"
        type="button"
        disabled={busy}
        onClick={() => void essential(space)}
      >
        {space.essential ? "Make it not essential" : "Make it essential"}
      </button>
    );

  const addSection = async (event: FormEvent) => {
    event.preventDefault();
    if (await send("/sections", { name: newName }, `Added ${newName.trim()}.`)) {
      setNewName("");
    }
  };

  return (
    <>
      {error && (
        <div className="notice notice-error" role="alert">
          <p>{error}</p>
        </div>
      )}
      <p className="visually-hidden" role="status">
        {status}
      </p>
      {layout.sections.length === 0 && (
        <section className="panel">
          <p className="empty">No sections yet.</p>
        </section>
      )}
      {layout.sections.map((section, i) => (
        <section
          key={section.id}
          className={`panel layout-section${isDrop({ section: section.id, before: null }) ? " layout-drop-target" : ""}`}
          aria-label={section.name}
          data-section={section.id}
          onDragOver={over({ section: section.id, before: null })}
          onDrop={dropped({ section: section.id, before: null })}
        >
          <div className="panel-bar layout-section-bar">
            <SectionName
              key={section.name}
              section={section}
              busy={busy}
              rename={(name) => send(`/sections/${section.id}`, { name }, "Section renamed.")}
            />
            <div className="layout-section-form">
              {i > 0 && (
                <button
                  className="button button-small button-quiet"
                  type="button"
                  disabled={busy}
                  onClick={() => void order(reordered(layout.sections, section.id, -1))}
                >
                  Move up
                </button>
              )}
              {i < layout.sections.length - 1 && (
                <button
                  className="button button-small button-quiet"
                  type="button"
                  disabled={busy}
                  onClick={() => void order(reordered(layout.sections, section.id, 1))}
                >
                  Move down
                </button>
              )}
              {confirming === section.id ? (
                <>
                  <button
                    className="button button-small"
                    type="button"
                    disabled={busy}
                    onClick={async () => {
                      if (await send(`/sections/${section.id}/delete`, {}, `Deleted ${section.name}.`)) {
                        setConfirming(null);
                      }
                    }}
                  >
                    Delete the section
                  </button>
                  <button
                    className="button button-small button-quiet"
                    type="button"
                    onClick={() => setConfirming(null)}
                  >
                    Keep it
                  </button>
                </>
              ) : (
                <button
                  className="button button-small button-quiet"
                  type="button"
                  onClick={() => setConfirming(section.id)}
                >
                  Delete section…
                </button>
              )}
            </div>
          </div>
          {confirming === section.id && (
            <p className="panel-text">
              {props.admin
                ? "The section goes from the default front page, and from the front page of every member who hasn’t arranged their own. Its spaces stay."
                : "The section goes from your front page. Its spaces stay."}
            </p>
          )}
          {section.spaces.length === 0 ? (
            <p className="empty">No spaces in this section. Drag one here.</p>
          ) : (
            <ul className="member-list layout-spaces">
              {section.spaces.map((space, j) => (
                <li
                  key={space.id}
                  draggable={!busy}
                  data-space={space.slug}
                  className={[
                    dragging === space.id ? "layout-dragging" : "",
                    isDrop({ section: section.id, before: space.id }) ? "layout-drop-before" : "",
                  ].join(" ")}
                  onDragStart={start(space)}
                  onDragEnd={end}
                  onDragOver={over({ section: section.id, before: space.id })}
                  onDrop={dropped({ section: section.id, before: space.id })}
                >
                  <span className="layout-drag" aria-hidden="true">
                    ⠿
                  </span>
                  <span className="admin-space-name">
                    <a href={`/c/${space.slug}`}>{space.name}</a> <Flags space={space} />
                  </span>
                  {j > 0 && (
                    <button
                      className="button button-small button-quiet"
                      type="button"
                      disabled={busy}
                      onClick={() => void place(space, section.id, j - 1)}
                    >
                      Move up
                    </button>
                  )}
                  {j < section.spaces.length - 1 && (
                    <button
                      className="button button-small button-quiet"
                      type="button"
                      disabled={busy}
                      onClick={() => void place(space, section.id, j + 1)}
                    >
                      Move down
                    </button>
                  )}
                  {moveTo(space, section.id)}
                  {essentialButton(space)}
                </li>
              ))}
            </ul>
          )}
        </section>
      ))}
      <section
        className={`panel layout-available${isDrop("off") ? " layout-drop-target" : ""}`}
        aria-labelledby="layout-available-heading"
        onDragOver={over("off")}
        onDrop={dropped("off")}
      >
        <h2 className="panel-bar" id="layout-available-heading">
          {props.admin ? "Not on the default front page" : "Not on your front page"}
        </h2>
        {layout.available.length === 0 ? (
          <p className="empty">Every space you can read is in a section.</p>
        ) : (
          <ul className="member-list layout-spaces">
            {layout.available.map((space) => (
              <li
                key={space.id}
                draggable={!busy}
                data-space={space.slug}
                className={dragging === space.id ? "layout-dragging" : ""}
                onDragStart={start(space)}
                onDragEnd={end}
              >
                <span className="layout-drag" aria-hidden="true">
                  ⠿
                </span>
                <span className="admin-space-name">
                  <a href={`/c/${space.slug}`}>{space.name}</a> <Flags space={space} />
                </span>
                {moveTo(space, null)}
                {essentialButton(space)}
              </li>
            ))}
          </ul>
        )}
        <p className="panel-foot">Drag a space into a section to put it on the front page, or here to take it off.</p>
      </section>
      <form className="panel" onSubmit={addSection}>
        <h2 className="panel-bar">New section</h2>
        <div className="form">
          <div className="field">
            <label htmlFor="new-section-name">Name</label>
            <input
              id="new-section-name"
              type="text"
              maxLength={80}
              required
              value={newName}
              onChange={(event) => setNewName(event.target.value)}
            />
          </div>
          <div className="form-actions">
            <button className="button" type="submit" disabled={busy}>
              Add section
            </button>
          </div>
        </div>
      </form>
    </>
  );
}
