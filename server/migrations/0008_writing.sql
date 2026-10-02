-- Writing (DESIGN.md §3.3–3.5): edit history, reactions and read markers.

-- Who made the current version of a post, when it isn't its author.
-- (Only authors edit in M4; moderators can edit from M7.)
ALTER TABLE posts ADD COLUMN edited_by bigint REFERENCES users (id);

-- Earlier versions of edited posts. Each row is one version as it was
-- before an edit replaced it: `rev` 1 is the original, and the post itself
-- holds the newest version. Edits within the grace period after posting
-- replace the original without adding a row.
CREATE TABLE post_revisions (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    post_id bigint NOT NULL REFERENCES posts (id),
    rev integer NOT NULL CHECK (rev >= 1),
    body_md text NOT NULL,
    body_html text NOT NULL,
    -- Who wrote this version.
    editor_id bigint NOT NULL REFERENCES users (id),
    -- When this version was written.
    created_at timestamptz NOT NULL,
    -- Redaction (M7) removes a version's text for everyone.
    redacted_at timestamptz,
    redacted_by bigint REFERENCES users (id),
    UNIQUE (post_id, rev),
    CHECK ((redacted_at IS NULL) = (redacted_by IS NULL))
);

-- The emoji members can react with, chosen by the admin.
CREATE TABLE reaction_types (
    emoji text PRIMARY KEY CHECK (char_length(emoji) BETWEEN 1 AND 16),
    position integer NOT NULL DEFAULT 0,
    enabled boolean NOT NULL DEFAULT true
);

INSERT INTO reaction_types (emoji, position) VALUES
    ('👍', 1), ('❤️', 2), ('😂', 3), ('😮', 4), ('😢', 5), ('🙏', 6);

CREATE TABLE reactions (
    post_id bigint NOT NULL REFERENCES posts (id),
    user_id bigint NOT NULL REFERENCES users (id),
    emoji text NOT NULL REFERENCES reaction_types (emoji) ON UPDATE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (post_id, user_id, emoji)
);

CREATE INDEX reactions_user ON reactions (user_id);

-- How far each member has read each thread: the highest post number that
-- has been shown to them. It only moves forward.
CREATE TABLE thread_reads (
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    thread_id bigint NOT NULL REFERENCES threads (id),
    last_read_seq integer NOT NULL CHECK (last_read_seq >= 0),
    PRIMARY KEY (user_id, thread_id)
);

CREATE INDEX thread_reads_thread ON thread_reads (thread_id);
