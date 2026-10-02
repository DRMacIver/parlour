-- Direct messages, blocks, watches and notifications (DESIGN.md §3.7, §4, §9).

-- A DM is a thread with visibility 'private' and no category; these are the
-- people in it. Someone who leaves keeps their row, with `left_at` set, and
-- can no longer read the conversation unless they are added again.
CREATE TABLE private_thread_participants (
    thread_id bigint NOT NULL REFERENCES threads (id),
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    joined_at timestamptz NOT NULL DEFAULT now(),
    left_at timestamptz,
    added_by bigint REFERENCES users (id) ON DELETE SET NULL,
    PRIMARY KEY (thread_id, user_id)
);

-- A member's conversations, for /messages and the read check.
CREATE INDEX private_thread_participants_user ON private_thread_participants (user_id, thread_id)
    WHERE left_at IS NULL;

-- Only DMs have participants.
CREATE FUNCTION participants_only_in_private_threads() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM threads WHERE id = NEW.thread_id AND visibility = 'private') THEN
        RAISE EXCEPTION 'thread % is not a private thread', NEW.thread_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER participants_only_in_private_threads BEFORE INSERT OR UPDATE OF thread_id
    ON private_thread_participants
    FOR EACH ROW EXECUTE FUNCTION participants_only_in_private_threads();

-- The second half of the one visibility condition: a private thread can be
-- read by its current participants and nobody else (not even staff, until
-- a DM is reported in M7). `thread_readable` never matches a private
-- thread, so lists of category threads can't include DMs; lookups of one
-- thread or post use `thread_readable(...) OR dm_readable(...)`.
CREATE FUNCTION dm_readable(thread_id bigint, visibility text, viewer_id bigint)
RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT visibility = 'private' AND viewer_id IS NOT NULL AND EXISTS (
        SELECT 1 FROM private_thread_participants p
        WHERE p.thread_id = dm_readable.thread_id AND p.user_id = viewer_id AND p.left_at IS NULL)
$$;

-- Who may start a conversation with a member, and which emails they want.
ALTER TABLE users
    ADD COLUMN dm_policy text NOT NULL DEFAULT 'everyone'
        CHECK (dm_policy IN ('everyone', 'interacted', 'nobody')),
    ADD COLUMN email_prefs jsonb NOT NULL DEFAULT '{}';

-- `user_id` doesn't want to hear from `blocked_user_id`.
CREATE TABLE blocks (
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    blocked_user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, blocked_user_id),
    CHECK (user_id <> blocked_user_id)
);

CREATE INDEX blocks_blocked ON blocks (blocked_user_id);

-- How closely a member follows a thread or a category. No row means
-- 'normal'.
CREATE TABLE watches (
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    thread_id bigint REFERENCES threads (id),
    category_id bigint REFERENCES categories (id) ON DELETE CASCADE,
    level text NOT NULL CHECK (level IN ('watching', 'normal', 'muted')),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (num_nonnulls(thread_id, category_id) = 1)
);

CREATE UNIQUE INDEX watches_thread ON watches (user_id, thread_id) WHERE thread_id IS NOT NULL;
CREATE UNIQUE INDEX watches_category ON watches (user_id, category_id) WHERE category_id IS NOT NULL;
CREATE INDEX watches_thread_level ON watches (thread_id, level) WHERE thread_id IS NOT NULL;
CREATE INDEX watches_category_level ON watches (category_id, level) WHERE category_id IS NOT NULL;

-- In-app notifications. Each is about one post (or, for moderation
-- notices, carries a message instead), and a member gets at most one
-- notification per post.
CREATE TABLE notifications (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('reply', 'mention', 'message', 'watched_thread',
                                       'watched_category', 'moderation')),
    actor_id bigint REFERENCES users (id) ON DELETE SET NULL,
    thread_id bigint REFERENCES threads (id),
    post_id bigint REFERENCES posts (id),
    message text CHECK (char_length(message) <= 1000),
    created_at timestamptz NOT NULL DEFAULT now(),
    read_at timestamptz,
    emailed_at timestamptz,
    CHECK (post_id IS NOT NULL OR kind = 'moderation'),
    CHECK (kind <> 'moderation' OR message IS NOT NULL)
);

CREATE UNIQUE INDEX notifications_one_per_post ON notifications (user_id, post_id) WHERE post_id IS NOT NULL;
CREATE INDEX notifications_user ON notifications (user_id, created_at DESC, id DESC);
CREATE INDEX notifications_unread ON notifications (user_id) WHERE read_at IS NULL;
CREATE INDEX notifications_thread ON notifications (thread_id, user_id) WHERE read_at IS NULL;
