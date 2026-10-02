-- Moderation (DESIGN.md §8): reports and the queue, posts held for
-- approval, the moderation log, staff notes, silences and suspensions,
-- moving posts between threads, and the addresses members use.

-- A silenced member can read but not write; a suspended one can only read
-- what visitors can, and contest the suspension. Each lasts until the time
-- given.
ALTER TABLE users
    ADD COLUMN silenced_until timestamptz,
    ADD COLUMN suspended_until timestamptz;

-- Who removed a post, when it wasn't its author (deleting) or always
-- (hiding, which only staff do).
ALTER TABLE posts
    ADD COLUMN deleted_by bigint REFERENCES users (id),
    ADD COLUMN hidden_by bigint REFERENCES users (id);

-- A thread whose posts were all moved into another one. Its address leads
-- there.
ALTER TABLE threads ADD COLUMN merged_into_thread_id bigint REFERENCES threads (id);

-- Posts moved to another thread take the next numbers there; the number
-- each had in its old thread is kept by one of these, which the old thread
-- shows in its place ("№ 12 was moved to …"), so that no page of either
-- thread changes which numbers it holds. A place in a thread holds either a
-- post or a move, never both.
CREATE TABLE post_moves (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    post_id bigint NOT NULL REFERENCES posts (id),
    from_thread_id bigint NOT NULL REFERENCES threads (id),
    from_seq integer NOT NULL CHECK (from_seq >= 1),
    to_thread_id bigint NOT NULL REFERENCES threads (id),
    moved_by bigint REFERENCES users (id),
    moved_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (from_thread_id, from_seq)
);

CREATE INDEX post_moves_post ON post_moves (post_id);

-- Everything moderators do, with their reason. `public` entries are about
-- public content (or about members, whose names are public) and are listed
-- at /moderation for everyone; the rest only for staff.
CREATE TABLE mod_actions (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    actor_id bigint NOT NULL REFERENCES users (id),
    action text NOT NULL,
    target_kind text NOT NULL CHECK (target_kind IN ('post', 'thread', 'user', 'revision', 'report', 'held_post')),
    target_id bigint NOT NULL,
    -- The member it was about, the thread and the post, where there are
    -- ones, for listing and for appeals.
    user_id bigint REFERENCES users (id),
    thread_id bigint REFERENCES threads (id),
    post_id bigint REFERENCES posts (id),
    reason text NOT NULL CHECK (char_length(reason) <= 500),
    public boolean NOT NULL,
    details jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX mod_actions_recent ON mod_actions (created_at DESC, id DESC);
CREATE INDEX mod_actions_public ON mod_actions (created_at DESC, id DESC) WHERE public;
CREATE INDEX mod_actions_user ON mod_actions (user_id, created_at DESC) WHERE user_id IS NOT NULL;

-- Reports (about a post or a member), messages sent with the contact form,
-- and appeals against moderators' actions: everything in the queue that
-- someone sent in.
CREATE TABLE reports (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind text NOT NULL CHECK (kind IN ('post', 'user', 'contact', 'appeal')),
    reporter_id bigint REFERENCES users (id) ON DELETE SET NULL,
    -- For the contact form: where to reply, if the sender gave an address.
    contact_email citext CHECK (char_length(contact_email) <= 254),
    post_id bigint REFERENCES posts (id),
    -- The member reported, or the author of the post reported, or the
    -- member appealing.
    user_id bigint REFERENCES users (id),
    -- The thread the post was in when reported. For a post in a
    -- conversation, this is what the report shows moderators: that
    -- conversation, and no other.
    thread_id bigint REFERENCES threads (id),
    -- For an appeal, the action contested.
    mod_action_id bigint REFERENCES mod_actions (id),
    reason text NOT NULL CHECK (reason IN ('spam', 'harassment', 'personal', 'inappropriate', 'other', 'contact', 'appeal')),
    details text NOT NULL DEFAULT '' CHECK (char_length(details) <= 2000),
    status text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved', 'dismissed')),
    resolved_by bigint REFERENCES users (id),
    resolution text CHECK (char_length(resolution) <= 500),
    resolved_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((kind = 'post') = (post_id IS NOT NULL)),
    CHECK (kind <> 'post' OR thread_id IS NOT NULL),
    CHECK (kind NOT IN ('user', 'appeal') OR user_id IS NOT NULL),
    CHECK ((kind = 'appeal') = (mod_action_id IS NOT NULL)),
    CHECK ((status = 'open') = (resolved_at IS NULL))
);

CREATE INDEX reports_open ON reports (created_at, id) WHERE status = 'open';
CREATE INDEX reports_thread ON reports (thread_id) WHERE thread_id IS NOT NULL;
CREATE INDEX reports_user ON reports (user_id) WHERE user_id IS NOT NULL;
-- A member reports a thing once while it is open, and contests an action
-- once.
CREATE UNIQUE INDEX reports_one_open_per_post ON reports (reporter_id, post_id)
    WHERE status = 'open' AND kind = 'post';
CREATE UNIQUE INDEX reports_one_open_per_user ON reports (reporter_id, user_id)
    WHERE status = 'open' AND kind = 'user';
CREATE UNIQUE INDEX reports_one_appeal_per_action ON reports (mod_action_id) WHERE kind = 'appeal';

-- Posts by new members waiting for a moderator (DESIGN.md §7): either a new
-- thread in a category (with a title, perhaps quote-forking a post) or a
-- reply in a thread. Nobody but the author and staff sees them until one
-- is approved, when it is posted for real.
CREATE TABLE held_posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id bigint NOT NULL REFERENCES users (id),
    category_id bigint REFERENCES categories (id) ON DELETE CASCADE,
    thread_id bigint REFERENCES threads (id),
    title text CHECK (char_length(title) <= 300),
    body_md text NOT NULL,
    reply_to_post_id bigint REFERENCES posts (id),
    forked_from_post_id bigint REFERENCES posts (id),
    status text NOT NULL DEFAULT 'held' CHECK (status IN ('held', 'approved', 'rejected')),
    -- Once approved, the post it became.
    post_id bigint REFERENCES posts (id),
    decided_by bigint REFERENCES users (id),
    decided_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK (num_nonnulls(category_id, thread_id) = 1),
    CHECK ((category_id IS NULL) = (title IS NULL)),
    CHECK ((status = 'held') = (decided_at IS NULL)),
    CHECK ((status = 'approved') = (post_id IS NOT NULL))
);

CREATE INDEX held_posts_waiting ON held_posts (created_at, id) WHERE status = 'held';
CREATE INDEX held_posts_author ON held_posts (author_id);

-- Staff-only notes about members.
CREATE TABLE user_notes (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    author_id bigint NOT NULL REFERENCES users (id),
    body text NOT NULL CHECK (char_length(body) BETWEEN 1 AND 2000),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX user_notes_user ON user_notes (user_id, created_at DESC);

-- The addresses each member has used, for spotting accounts that share
-- one. Kept only for the retention period set in the moderation settings.
CREATE TABLE user_ips (
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    ip text NOT NULL CHECK (char_length(ip) <= 64),
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, ip)
);

CREATE INDEX user_ips_ip ON user_ips (ip);
CREATE INDEX user_ips_last_seen ON user_ips (last_seen_at);

-- A moderation notice can be about an action, which its member can then
-- contest.
ALTER TABLE notifications ADD COLUMN mod_action_id bigint REFERENCES mod_actions (id);

-- Reporting a message reveals that conversation, and no other, to staff:
-- a conversation is readable by its current participants, and by staff
-- once a post in it has been reported. (Suspended members are given no
-- viewer id here, so they read no conversations.)
CREATE OR REPLACE FUNCTION dm_readable(thread_id bigint, visibility text, viewer_id bigint)
RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT visibility = 'private' AND viewer_id IS NOT NULL AND (
        EXISTS (
            SELECT 1 FROM private_thread_participants p
            WHERE p.thread_id = dm_readable.thread_id AND p.user_id = viewer_id AND p.left_at IS NULL)
        OR (EXISTS (SELECT 1 FROM reports r WHERE r.thread_id = dm_readable.thread_id AND r.kind = 'post')
            AND EXISTS (SELECT 1 FROM users u
                        WHERE u.id = viewer_id AND u.role IN ('moderator', 'admin') AND u.deleted_at IS NULL)))
$$;
