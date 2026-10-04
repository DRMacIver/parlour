-- Per-thread permissions (DESIGN.md §3.4, milestone 14).
--
-- A thread may carry its own `read` and `reply` restriction: the groups,
-- lists and people named, in arrays as a grant holds them, or NULL when
-- the thread simply inherits its space's audience. The effective audience
-- is the intersection of the space's grant and the thread's restriction,
-- so a thread can only ever be narrower than its space.
--
-- `threads.visibility` becomes the effective visibility: a thread with a
-- read restriction is `restricted` whatever its space's read grant. The
-- space's own visibility is kept beside it in `threads.space_visibility`
-- (what `threads.visibility` held until now), so that the access layer's
-- one condition can check the space's grant and then the thread's names
-- without a join. Both are set by the trigger on every insert, move and
-- change of restriction, and follow the space's read grant when it
-- changes; the space's row is still locked FOR SHARE (migration 0003).

ALTER TABLE threads
    ADD COLUMN read_group_ids bigint[],
    ADD COLUMN read_user_ids bigint[],
    ADD COLUMN reply_group_ids bigint[],
    ADD COLUMN reply_user_ids bigint[],
    ADD COLUMN space_visibility text;

UPDATE threads SET space_visibility = visibility;

ALTER TABLE threads
    ALTER COLUMN space_visibility SET NOT NULL,
    ADD CONSTRAINT threads_space_visibility_check
        CHECK (space_visibility IN ('public', 'members', 'restricted', 'private')),
    -- A restriction names groups and people together, or is absent.
    ADD CONSTRAINT threads_read_restriction_check
        CHECK ((read_group_ids IS NULL) = (read_user_ids IS NULL)),
    ADD CONSTRAINT threads_reply_restriction_check
        CHECK ((reply_group_ids IS NULL) = (reply_user_ids IS NULL)),
    -- A conversation's readers are its participants (DESIGN.md §3.4).
    ADD CONSTRAINT threads_restrictions_in_spaces_check
        CHECK (space_id IS NOT NULL OR (read_group_ids IS NULL AND reply_group_ids IS NULL));

CREATE INDEX threads_read_groups ON threads USING gin (read_group_ids);
CREATE INDEX threads_reply_groups ON threads USING gin (reply_group_ids);

CREATE OR REPLACE FUNCTION threads_set_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.space_id IS NULL THEN
        NEW.space_visibility := 'private';
        NEW.visibility := 'private';
    ELSE
        SELECT visibility INTO NEW.space_visibility FROM spaces WHERE id = NEW.space_id FOR SHARE;
        NEW.visibility := CASE WHEN NEW.read_user_ids IS NOT NULL THEN 'restricted'
                               ELSE NEW.space_visibility END;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER threads_set_visibility ON threads;
CREATE TRIGGER threads_set_visibility
    BEFORE INSERT OR UPDATE OF space_id, visibility, space_visibility, read_group_ids, read_user_ids
    ON threads
    FOR EACH ROW EXECUTE FUNCTION threads_set_visibility();

-- The one visibility condition (DESIGN.md §6), now over the thread's row:
-- `vis` lists the visibility levels the viewer reads everywhere (staff
-- read `restricted` too, until milestone 15), `restricted_space_ids` the
-- restricted spaces whose read grant lets them in, `reader` who they are
-- (NULL for visitors and suspended members) and `reader_groups` the
-- groups and lists that let them in. A thread with its own restriction is
-- read by those who can read its space and whom it names.
DROP FUNCTION thread_readable(text, bigint, text[], bigint[]);
CREATE FUNCTION thread_readable(t threads, vis text[], restricted_space_ids bigint[],
                                reader bigint, reader_groups bigint[])
RETURNS boolean LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT t.visibility = ANY (vis)
        OR (t.visibility = 'restricted'
            AND (t.space_visibility = ANY (vis) OR t.space_id = ANY (restricted_space_ids))
            AND (t.read_user_ids IS NULL
                 OR coalesce(reader = ANY (t.read_user_ids), false)
                 OR t.read_group_ids && coalesce(reader_groups, '{}')))
$$;

DROP FUNCTION unread_notification_count(bigint, text[], bigint[], bigint);
CREATE FUNCTION unread_notification_count(member bigint, vis text[], restricted_space_ids bigint[],
                                          dm_viewer bigint, reader_groups bigint[])
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT count(*)
    FROM notifications n
    LEFT JOIN posts p ON p.id = n.post_id
    LEFT JOIN threads t ON t.id = p.thread_id
    WHERE n.user_id = member AND n.read_at IS NULL
      AND (n.post_id IS NULL
           OR (p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
               AND (thread_readable(t, vis, restricted_space_ids, dm_viewer, reader_groups)
                    OR dm_readable(t.id, t.visibility, dm_viewer))))
      AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                      WHERE b.user_id = member AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
$$;

-- Whether lists of spaces show a space (DESIGN.md §3.2): a topic space
-- always, a feed to its owner (when `owner_viewer` is them) and, to anyone,
-- once it has a thread they can read.
DROP FUNCTION space_in_view(text, bigint, bigint, bigint);
CREATE FUNCTION space_in_view(kind text, owner bigint, space bigint, owner_viewer bigint,
                              vis text[], restricted_space_ids bigint[],
                              reader bigint, reader_groups bigint[])
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT kind = 'topic'
        OR owner = owner_viewer
        OR EXISTS (SELECT 1 FROM threads t
                   WHERE t.space_id = space AND t.deleted_at IS NULL
                     AND thread_readable(t, vis, restricted_space_ids, reader, reader_groups))
$$;

-- Post counts (migration 0019) are kept per member and space for threads
-- that inherit their space's audience, and per member and thread for
-- threads with their own read restriction, so that summing what a viewer
-- can read never counts a post in a thread they can't.
ALTER TABLE user_space_post_counts
    ADD COLUMN thread_id bigint REFERENCES threads (id) ON DELETE CASCADE,
    DROP CONSTRAINT user_space_post_counts_pkey,
    ADD CONSTRAINT user_space_post_counts_key UNIQUE NULLS NOT DISTINCT (user_id, space_id, thread_id);

DROP FUNCTION adjust_post_count(bigint, bigint, bigint);
CREATE FUNCTION adjust_post_count(member bigint, space bigint, thread bigint, delta bigint)
RETURNS void LANGUAGE sql AS $$
    INSERT INTO user_space_post_counts (user_id, space_id, thread_id, posts)
    VALUES (member, space, thread, delta)
    ON CONFLICT (user_id, space_id, thread_id)
    DO UPDATE SET posts = user_space_post_counts.posts + EXCLUDED.posts
$$;

-- Where a post in `thread` counts: its space, and the thread itself if it
-- has its own read restriction; nothing if it doesn't count.
DROP FUNCTION post_counted_in(bigint, timestamptz, timestamptz);
CREATE FUNCTION post_counted_in(thread bigint, deleted timestamptz, hidden timestamptz,
                                OUT space bigint, OUT restricted_thread bigint)
LANGUAGE sql STABLE AS $$
    SELECT t.space_id, CASE WHEN t.read_user_ids IS NOT NULL THEN t.id END
    FROM threads t
    WHERE t.id = thread AND t.deleted_at IS NULL AND deleted IS NULL AND hidden IS NULL
$$;

CREATE OR REPLACE FUNCTION count_post_changes() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    counted record;
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        SELECT * INTO counted FROM post_counted_in(OLD.thread_id, OLD.deleted_at, OLD.hidden_at);
        IF counted.space IS NOT NULL THEN
            PERFORM adjust_post_count(OLD.author_id, counted.space, counted.restricted_thread, -1);
        END IF;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        SELECT * INTO counted FROM post_counted_in(NEW.thread_id, NEW.deleted_at, NEW.hidden_at);
        IF counted.space IS NOT NULL THEN
            PERFORM adjust_post_count(NEW.author_id, counted.space, counted.restricted_thread, 1);
        END IF;
    END IF;
    RETURN NULL;
END
$$;

-- A thread moving space, being deleted or restored, or gaining or losing
-- its read restriction moves all of its counted posts at once.
CREATE OR REPLACE FUNCTION count_thread_changes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.space_id IS NOT NULL AND OLD.deleted_at IS NULL THEN
        INSERT INTO user_space_post_counts (user_id, space_id, thread_id, posts)
        SELECT p.author_id, OLD.space_id,
               CASE WHEN OLD.read_user_ids IS NOT NULL THEN OLD.id END, -count(*)
        FROM posts p
        WHERE p.thread_id = OLD.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
        GROUP BY p.author_id
        ON CONFLICT (user_id, space_id, thread_id)
        DO UPDATE SET posts = user_space_post_counts.posts + EXCLUDED.posts;
    END IF;
    IF NEW.space_id IS NOT NULL AND NEW.deleted_at IS NULL THEN
        INSERT INTO user_space_post_counts (user_id, space_id, thread_id, posts)
        SELECT p.author_id, NEW.space_id,
               CASE WHEN NEW.read_user_ids IS NOT NULL THEN NEW.id END, count(*)
        FROM posts p
        WHERE p.thread_id = NEW.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
        GROUP BY p.author_id
        ON CONFLICT (user_id, space_id, thread_id)
        DO UPDATE SET posts = user_space_post_counts.posts + EXCLUDED.posts;
    END IF;
    RETURN NULL;
END
$$;

DROP TRIGGER threads_count_changes ON threads;
CREATE TRIGGER threads_count_changes
AFTER UPDATE OF space_id, deleted_at, read_user_ids ON threads
FOR EACH ROW
WHEN (OLD.space_id IS DISTINCT FROM NEW.space_id
      OR OLD.deleted_at IS DISTINCT FROM NEW.deleted_at
      OR (OLD.read_user_ids IS NULL) <> (NEW.read_user_ids IS NULL))
EXECUTE FUNCTION count_thread_changes();

-- A group or list named in a thread's restriction can't be deleted, as one
-- named in a space's grant can't (DESIGN.md §3.3).
CREATE OR REPLACE FUNCTION groups_refuse_delete_in_grants() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM space_grants WHERE group_ids @> ARRAY[OLD.id]) THEN
        RAISE EXCEPTION 'group % is named in a space''s grants', OLD.id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF EXISTS (SELECT 1 FROM threads
               WHERE read_group_ids @> ARRAY[OLD.id] OR reply_group_ids @> ARRAY[OLD.id]) THEN
        RAISE EXCEPTION 'group % is named in a thread''s restriction', OLD.id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    RETURN OLD;
END;
$$;
