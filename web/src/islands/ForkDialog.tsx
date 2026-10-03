// The quote-fork dialog: "Quote in new thread" opens a dialog on the page
// instead of the full page for it. The dialog's form is the same form that
// page has, posted the same way, so a refusal (a title too long, say)
// comes back as that page, with what was typed kept.

import { useEffect, useRef, useState } from "react";
import { ApiError, getJson } from "../lib/api";
import Composer from "./Composer";

export interface ForkDialogProps {
  endpoint: string;
}

interface Fork {
  action: string;
  source: { seq: number; author: string; thread: string; href: string; html: string };
  spaces: { slug: string; label: string; selected: boolean }[];
}

export default function ForkDialog(props: ForkDialogProps) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [fork, setFork] = useState<Fork | null>(null);
  const [opened, setOpened] = useState(0);

  useEffect(() => {
    const follow = async (event: MouseEvent) => {
      if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) {
        return;
      }
      const link = (event.target as Element | null)?.closest<HTMLAnchorElement>("a[data-fork]");
      if (!link) {
        return;
      }
      event.preventDefault();
      try {
        const loaded = await getJson<Fork>(props.endpoint.replace("{id}", link.dataset.fork ?? ""));
        if (loaded.spaces.length === 0) {
          // The page explains why not.
          window.location.href = link.href;
          return;
        }
        setFork(loaded);
        setOpened((n) => n + 1);
      } catch (failure) {
        if (!(failure instanceof ApiError) || failure.status !== 401) {
          window.location.href = link.href;
        }
      }
    };
    document.addEventListener("click", follow);
    return () => document.removeEventListener("click", follow);
  }, [props.endpoint]);

  useEffect(() => {
    if (opened > 0 && dialog.current && !dialog.current.open) {
      dialog.current.showModal();
    }
  }, [opened]);

  return (
    <dialog className="fork-dialog-box" ref={dialog} aria-labelledby="fork-dialog-heading">
      {fork && (
        <form className="composer" method="post" action={fork.action} key={opened}>
          <h2 className="panel-bar" id="fork-dialog-heading">
            Start a new thread from № {fork.source.seq}
          </h2>
          <div className="form">
            <figure className="embed">
              <figcaption className="embed-caption">
                <span className="embed-label">Quoting</span>
                <span>
                  {fork.source.author} in <a href={fork.source.href}>{fork.source.thread}</a>
                </span>
              </figcaption>
              <div className="embed-body">
                <div className="embed-content">
                  <div className="post-body" dangerouslySetInnerHTML={{ __html: fork.source.html }} />
                </div>
              </div>
            </figure>
            <div className="field">
              <label htmlFor="fork-title">Title</label>
              <input type="text" id="fork-title" name="title" maxLength={150} required />
            </div>
            <div className="field">
              <label htmlFor="fork-space">Space</label>
              <select
                id="fork-space"
                name="space"
                defaultValue={fork.spaces.find((c) => c.selected)?.slug ?? fork.spaces[0].slug}
              >
                {fork.spaces.map((c) => (
                  <option key={c.slug} value={c.slug}>
                    {c.label}
                  </option>
                ))}
              </select>
            </div>
            <div className="field">
              <Composer
                id="fork-body"
                name="body"
                label="Your opening post"
                initial=""
                rows={6}
                quotes={false}
                preview="/api/preview"
                upload="/api/uploads"
              />
            </div>
            <p className="form-note">
              The quoted post stays where it is and appears at the top of the new thread. Only spaces at
              least as private as its own are offered.
            </p>
            <div className="form-actions">
              <button className="button" type="submit">
                Create thread
              </button>
              <button className="button button-quiet" type="button" onClick={() => dialog.current?.close()}>
                Cancel
              </button>
            </div>
          </div>
        </form>
      )}
    </dialog>
  );
}
