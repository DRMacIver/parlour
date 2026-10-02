// The Notifications and Messages links in the navigation bar, with badges
// kept up to date over the server's live stream, and a menu of recent
// notifications. Without it the links are the same, with the counts as
// they were when the page was drawn.

import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, getJson, postJson } from "../lib/api";

export interface NotificationsProps {
  notifications: number;
  messages: number;
  events: string;
  list: string;
  markRead: string;
  element?: HTMLElement;
}

interface Item {
  id: number;
  actor: string | null;
  text: string;
  title: string | null;
  href: string;
  read: boolean;
  at: string;
  iso: string;
}

interface Menu {
  unread: number;
  messages: number;
  items: Item[];
}

function Badge({ count, label, kind }: { count: number; label: string; kind: string }) {
  if (count <= 0) {
    return null;
  }
  return (
    <>
      {" "}
      <span className="count-badge" data-count={kind}>
        {count}
      </span>
      <span className="visually-hidden"> {label}</span>
    </>
  );
}

export default function Notifications(props: NotificationsProps) {
  // Which link is the current page, as the server marked it.
  const [current] = useState(() => {
    const link = props.element?.querySelector<HTMLAnchorElement>('a[aria-current="page"]');
    return link?.getAttribute("href") ?? null;
  });
  const [counts, setCounts] = useState({ notifications: props.notifications, messages: props.messages });
  const [open, setOpen] = useState(false);
  const [menu, setMenu] = useState<Menu | null>(null);
  const [error, setError] = useState<string | null>(null);
  const container = useRef<HTMLSpanElement>(null);
  const button = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    const source = new EventSource(props.events);
    source.addEventListener("counts", (event) => {
      const next = JSON.parse((event as MessageEvent<string>).data) as {
        notifications: number;
        messages: number;
      };
      setCounts(next);
      setMenu(null);
    });
    return () => source.close();
  }, [props.events]);

  const load = useCallback(async () => {
    try {
      const fetched = await getJson<Menu>(props.list);
      setMenu(fetched);
      setCounts({ notifications: fetched.unread, messages: fetched.messages });
      setError(null);
    } catch (failure) {
      setError(failure instanceof ApiError ? failure.message : "Notifications didn’t load.");
    }
  }, [props.list]);

  useEffect(() => {
    if (open && menu === null) {
      void load();
    }
  }, [open, menu, load]);

  // Close on Escape or a click elsewhere.
  useEffect(() => {
    if (!open) {
      return;
    }
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setOpen(false);
        button.current?.focus();
      }
    };
    const onClick = (event: MouseEvent) => {
      if (!container.current?.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("click", onClick);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("click", onClick);
    };
  }, [open]);

  const markAllRead = async () => {
    try {
      const result = await postJson<{ unread: number; messages: number }>(props.markRead, {});
      setCounts({ notifications: result.unread, messages: result.messages });
      setMenu((m) => (m ? { ...m, unread: 0, items: m.items.map((i) => ({ ...i, read: true })) } : m));
    } catch (failure) {
      setError(failure instanceof ApiError ? failure.message : "That didn’t work. Try again.");
    }
  };

  return (
    <span className="notifications-island" ref={container}>
      <a
        className="nav-count"
        href="/notifications"
        aria-current={current === "/notifications" ? "page" : undefined}
      >
        Notifications
        <Badge count={counts.notifications} label="unread" kind="notifications" />
      </a>
      <button
        ref={button}
        type="button"
        className="nav-button nav-menu-button"
        aria-expanded={open}
        aria-controls="notifications-menu"
        aria-label="Recent notifications"
        onClick={() => setOpen((o) => !o)}
      >
        ▾
      </button>
      <a className="nav-count" href="/messages" aria-current={current === "/messages" ? "page" : undefined}>
        Messages
        <Badge count={counts.messages} label="with new messages" kind="messages" />
      </a>
      {open && (
        <div className="notifications-menu" id="notifications-menu" role="region" aria-label="Recent notifications">
          <div className="notifications-menu-head">
            <span>Recent notifications</span>
            <span className="spacer" />
            {menu && menu.unread > 0 && (
              <button type="button" className="link-button" onClick={markAllRead}>
                Mark all read
              </button>
            )}
          </div>
          {error && (
            <p className="composer-error" role="alert">
              {error}
            </p>
          )}
          {menu === null && !error && <p className="empty">Loading…</p>}
          {menu && menu.items.length === 0 && <p className="empty">Nothing yet.</p>}
          {menu && menu.items.length > 0 && (
            <ul className="notifications-menu-list">
              {menu.items.map((item) => (
                <li key={item.id} className={item.read ? undefined : "notification-unread"}>
                  <a href={item.href}>
                    {!item.read && <span className="visually-hidden">Unread: </span>}
                    {item.actor && <strong>{item.actor}</strong>} {item.text}
                    {item.title && <> {item.title}</>}
                  </a>
                  <time className="row-meta" dateTime={item.iso}>
                    {item.at}
                  </time>
                </li>
              ))}
            </ul>
          )}
          <a className="notifications-menu-all" href="/notifications">
            All notifications →
          </a>
        </div>
      )}
    </span>
  );
}
