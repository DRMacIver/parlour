// A post's reactions: toggling one, or adding one from the menu, without
// reloading the page. It replaces the small forms that do the same with a
// reload.

import { useRef, useState } from "react";
import { ApiError, postJson } from "../lib/api";

export interface Reaction {
  emoji: string;
  count: number;
  mine: boolean;
  names: string[];
}

export interface ReactionsProps {
  post: number;
  reactions: Reaction[];
  types: string[];
  endpoint: string;
  who: string;
}

/** "Ruth, Jonah and Maya", as the server writes lists of names. */
export function names(list: string[]): string {
  if (list.length <= 1) {
    return list.join("");
  }
  return `${list.slice(0, -1).join(", ")} and ${list[list.length - 1]}`;
}

export default function Reactions(props: ReactionsProps) {
  const [reactions, setReactions] = useState(props.reactions);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const menu = useRef<HTMLDetailsElement>(null);

  const send = async (emoji: string, action: "add" | "remove") => {
    if (busy) {
      return;
    }
    setBusy(true);
    try {
      const result = await postJson<{ reactions: Reaction[] }>(props.endpoint, { emoji, action });
      setReactions(result.reactions);
      setError(null);
    } catch (failure) {
      setError(failure instanceof ApiError ? failure.message : "That didn’t work. Try again.");
    } finally {
      setBusy(false);
      if (menu.current) {
        menu.current.open = false;
      }
    }
  };

  return (
    <>
      {reactions.map((r) => (
        <button
          key={r.emoji}
          className={`reaction${r.mine ? " reaction-mine" : ""}`}
          type="button"
          aria-pressed={r.mine}
          title={names(r.names)}
          disabled={busy}
          onClick={() => send(r.emoji, r.mine ? "remove" : "add")}
        >
          <span aria-hidden="true">{r.emoji}</span> <span className="reaction-count">{r.count}</span>
          <span className="visually-hidden"> reactions; {r.mine ? "take yours back" : "add yours"}</span>
        </button>
      ))}
      {props.types.length > 0 && (
        <details className="react-menu" ref={menu}>
          <summary>react</summary>
          <div className="react-choices">
            {props.types.map((emoji) => (
              <button
                key={emoji}
                className="reaction"
                type="button"
                value={emoji}
                aria-label={`React with ${emoji}`}
                disabled={busy}
                onClick={() => send(emoji, "add")}
              >
                {emoji}
              </button>
            ))}
          </div>
        </details>
      )}
      {reactions.length > 0 && (
        <a className="reaction-who" href={props.who}>
          who reacted
        </a>
      )}
      {error && (
        <span className="reaction-error" role="alert">
          {error}
        </span>
      )}
    </>
  );
}
