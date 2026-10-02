// The composer: a post's textarea, with a preview rendered by the server,
// images uploaded by pasting, dropping or choosing them, and (on thread
// and conversation pages) "reply" and "quote" links that fill it in
// instead of opening the full reply page. It replaces the plain textarea,
// and hides the form's own Preview button, which does the same by
// reloading the page.

import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, getJson, postForm, postJson } from "../lib/api";
import { insertAt, quoteSelection } from "../lib/text";

export interface ComposerProps {
  id: string;
  quotes: boolean;
  preview: string;
  upload: string;
  /** The element the server rendered, which holds the plain textarea. */
  element?: HTMLElement;
  /** For composers made by other islands rather than by the server. */
  name?: string;
  label?: string;
  initial?: string;
  rows?: number;
}

interface ReplyTo {
  post: number;
  seq: number;
  username: string;
}

interface Source {
  post: number;
  thread: number;
  seq: number;
  username: string;
  quote: string;
}

/** What the server rendered: the textarea's name, text and label. */
function fromServer(props: ComposerProps) {
  const textarea = props.element?.querySelector("textarea");
  const label = props.element?.querySelector("label");
  return {
    name: props.name ?? textarea?.name ?? "body",
    value: props.initial ?? textarea?.value ?? "",
    rows: props.rows ?? textarea?.rows ?? 8,
    label: props.label ?? label?.textContent ?? "Post",
    labelClass: label?.className ?? "",
    required: textarea?.required ?? true,
  };
}

export default function Composer(props: ComposerProps) {
  const [initial] = useState(() => fromServer(props));
  const [text, setText] = useState(initial.value);
  const [previewHtml, setPreviewHtml] = useState<string | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [uploading, setUploading] = useState(0);
  const [replyTo, setReplyTo] = useState<ReplyTo | null>(null);
  const [dragging, setDragging] = useState(false);
  const textarea = useRef<HTMLTextAreaElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const root = useRef<HTMLDivElement>(null);

  // The form's own Preview button reloads the page; this composer
  // previews in place instead.
  useEffect(() => {
    const form = root.current?.closest("form");
    const buttons = Array.from(
      form?.querySelectorAll<HTMLButtonElement>('button[type="submit"][name="preview"]') ?? [],
    );
    for (const button of buttons) {
      button.hidden = true;
    }
    return () => {
      for (const button of buttons) {
        button.hidden = false;
      }
    };
  }, []);

  /** Puts text in at the cursor (or the end), keeping what's there. */
  const insert = useCallback((addition: string, atEnd = false) => {
    const area = textarea.current;
    setText((current) => {
      const at = atEnd || !area ? current.length : area.selectionStart;
      const { text: next, cursor } = insertAt(current, at, addition);
      requestAnimationFrame(() => {
        area?.focus();
        area?.setSelectionRange(cursor, cursor);
      });
      return next;
    });
    setPreviewing(false);
  }, []);

  const uploadFiles = useCallback(
    async (files: File[]) => {
      const images = files.filter((file) => file.type.startsWith("image/"));
      for (const file of images) {
        const placeholder = `![Uploading ${file.name.replace(/[[\]]/g, "")}…]()`;
        insert(placeholder);
        setUploading((n) => n + 1);
        try {
          const form = new FormData();
          form.append("file", file);
          const result = await postForm<{ markdown: string }>(props.upload, form);
          setText((current) => current.replace(placeholder, result.markdown));
          setError(null);
        } catch (failure) {
          setText((current) => current.replace(placeholder, ""));
          setError(failure instanceof ApiError ? failure.message : "The image didn’t upload. Try again.");
        } finally {
          setUploading((n) => n - 1);
        }
      }
    },
    [insert, props.upload],
  );

  // Reply and quote links on the page fill this composer in.
  useEffect(() => {
    if (!props.quotes) {
      return;
    }
    let selected: { post: string; text: string } | null = null;
    const remember = (event: PointerEvent) => {
      const link = (event.target as Element | null)?.closest<HTMLAnchorElement>("a[data-quote]");
      selected = link ? { post: link.dataset.quote ?? "", text: quoteSelection(link) } : null;
    };
    const follow = async (event: MouseEvent) => {
      if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) {
        return;
      }
      const link = (event.target as Element | null)?.closest<HTMLAnchorElement>(
        "a[data-quote], a[data-reply-to]",
      );
      if (!link) {
        return;
      }
      event.preventDefault();
      const id = link.dataset.quote ?? link.dataset.replyTo ?? "";
      try {
        const source = await getJson<Source>(`/api/posts/${id}/source`);
        setReplyTo({ post: source.post, seq: source.seq, username: source.username });
        if (link.dataset.quote !== undefined) {
          const chosen = selected?.post === id ? selected.text : "";
          insert(
            chosen ? `> @${source.username} wrote:\n${chosen}\n\n` : source.quote,
            true,
          );
        } else {
          textarea.current?.focus();
        }
        root.current?.scrollIntoView({ block: "center", behavior: "smooth" });
      } catch {
        // Fall back to the full reply page.
        window.location.href = link.href;
      }
    };
    document.addEventListener("pointerdown", remember, true);
    document.addEventListener("click", follow);
    return () => {
      document.removeEventListener("pointerdown", remember, true);
      document.removeEventListener("click", follow);
    };
  }, [insert, props.quotes]);

  const showPreview = async () => {
    setPreviewing(true);
    setPreviewHtml(null);
    try {
      const result = await postJson<{ html: string }>(props.preview, { body: text });
      setPreviewHtml(result.html);
      setError(null);
    } catch (failure) {
      setPreviewing(false);
      setError(failure instanceof ApiError ? failure.message : "The preview didn’t load. Try again.");
    }
  };

  const hintId = `${props.id}-hint`;
  return (
    <div
      ref={root}
      className={`composer-island${dragging ? " composer-dragging" : ""}`}
      onDragOver={(event) => {
        if (event.dataTransfer.types.includes("Files")) {
          event.preventDefault();
          setDragging(true);
        }
      }}
      onDragLeave={() => setDragging(false)}
      onDrop={(event) => {
        setDragging(false);
        if (event.dataTransfer.files.length > 0) {
          event.preventDefault();
          void uploadFiles(Array.from(event.dataTransfer.files));
        }
      }}
    >
      <div className="composer-bar">
        <label className={initial.labelClass} htmlFor={props.id}>
          {initial.label}
        </label>
        <span className="spacer" />
        <div className="composer-tabs" role="group" aria-label="Composer">
          <button
            type="button"
            className="composer-tab"
            aria-pressed={!previewing}
            onClick={() => setPreviewing(false)}
          >
            Write
          </button>
          <button type="button" className="composer-tab" aria-pressed={previewing} onClick={showPreview}>
            Preview
          </button>
          <button type="button" className="composer-tab" onClick={() => fileInput.current?.click()}>
            Add image
          </button>
        </div>
        <input
          ref={fileInput}
          className="visually-hidden"
          type="file"
          accept="image/png,image/jpeg,image/gif,image/webp"
          multiple
          tabIndex={-1}
          aria-hidden="true"
          onChange={(event) => {
            void uploadFiles(Array.from(event.target.files ?? []));
            event.target.value = "";
          }}
        />
      </div>
      {replyTo && (
        <p className="composer-reply-to">
          <input type="hidden" name="reply_to" value={replyTo.post} />
          In reply to № {replyTo.seq} by @{replyTo.username}{" "}
          <button type="button" className="link-button" onClick={() => setReplyTo(null)}>
            not in reply
          </button>
        </p>
      )}
      <textarea
        ref={textarea}
        id={props.id}
        name={initial.name}
        rows={initial.rows}
        required={initial.required && !previewing}
        className={previewing ? "composer-hidden" : undefined}
        aria-describedby={hintId}
        value={text}
        onChange={(event) => setText(event.target.value)}
        onPaste={(event) => {
          const files = Array.from(event.clipboardData.files);
          if (files.some((file) => file.type.startsWith("image/"))) {
            event.preventDefault();
            void uploadFiles(files);
          }
        }}
      />
      {previewing && (
        <div className="preview composer-preview" aria-live="polite">
          {previewHtml === null ? (
            <p className="empty">Rendering…</p>
          ) : (
            <div className="post-body" dangerouslySetInnerHTML={{ __html: previewHtml }} />
          )}
        </div>
      )}
      {error && (
        <p className="composer-error" role="alert">
          {error}
        </p>
      )}
      <span className="field-hint" id={hintId}>
        Markdown supported. Paste or drop images to add them
        {uploading > 0 ? ` (uploading ${uploading})` : ""}.
      </span>
    </div>
  );
}
