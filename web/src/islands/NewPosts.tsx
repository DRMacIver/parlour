// The "N new posts" bar: follows the thread's live stream and, when
// others post, offers a link to the first new post. Without it, the page
// has a plain link that finds new posts by reloading.

import { useEffect, useState } from "react";

export interface NewPostsProps {
  events: string;
  lastSeq: number;
}

interface PostEvent {
  thread: number;
  post: number;
  seq: number;
}

export default function NewPosts(props: NewPostsProps) {
  const [fresh, setFresh] = useState<PostEvent[]>([]);
  const [dismissed, setDismissed] = useState(0);

  useEffect(() => {
    const source = new EventSource(props.events);
    source.addEventListener("post", (event) => {
      const post = JSON.parse((event as MessageEvent<string>).data) as PostEvent;
      if (post.seq > props.lastSeq) {
        setFresh((seen) => (seen.some((p) => p.post === post.post) ? seen : [...seen, post]));
      }
    });
    return () => source.close();
  }, [props.events, props.lastSeq]);

  const waiting = fresh.length - dismissed;
  const first =
    waiting > 0 ? fresh.slice(dismissed).reduce((a, b) => (a.seq <= b.seq ? a : b)) : null;
  // The live region is there from the start, so that screen readers
  // announce what appears in it.
  return (
    <div role="status" aria-live="polite">
      {first && (
        <div className="new-posts-bar">
          <a href={`/p/${first.post}`}>
            {waiting === 1 ? "1 new post" : `${waiting} new posts`} — show
          </a>
          <span className="spacer" />
          <button type="button" className="link-button" onClick={() => setDismissed(fresh.length)}>
            dismiss
          </button>
        </div>
      )}
    </div>
  );
}
