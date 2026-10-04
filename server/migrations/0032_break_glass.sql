-- Break glass, first step (milestone 15): staff read only what their own
-- memberships allow, and anything beyond that through a logged grant
-- (DESIGN.md §1 principle 3, §3.1, §6, §8).
--
-- A grant is a `staff_access` row: who, the thread or the whole space it
-- opens, the report that led to it if any, why, and until when. Every
-- page shown under it adds a `staff_access_views` row. Both tables are
-- append-only: the application's role has no UPDATE, DELETE or TRUNCATE
-- on them, and a trigger refuses all three whoever asks, so the record
-- can't be edited from inside the application.

CREATE TABLE staff_access (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    actor_id bigint NOT NULL REFERENCES users (id),
    scope text NOT NULL CHECK (scope IN ('thread', 'space')),
    -- A conversation is a thread too.
    thread_id bigint REFERENCES threads (id),
    space_id bigint REFERENCES spaces (id),
    report_id bigint REFERENCES reports (id),
    reason text NOT NULL CHECK (char_length(btrim(reason)) BETWEEN 1 AND 500),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    CONSTRAINT staff_access_scope_thread CHECK ((scope = 'thread') = (thread_id IS NOT NULL)),
    CONSTRAINT staff_access_scope_space CHECK ((scope = 'space') = (space_id IS NOT NULL)),
    CONSTRAINT staff_access_expires_later CHECK (expires_at > created_at)
);
CREATE INDEX staff_access_actor ON staff_access (actor_id, expires_at);
CREATE INDEX staff_access_thread ON staff_access (thread_id) WHERE thread_id IS NOT NULL;
CREATE INDEX staff_access_space ON staff_access (space_id) WHERE space_id IS NOT NULL;

CREATE TABLE staff_access_views (
    access_id bigint NOT NULL REFERENCES staff_access (id),
    path text NOT NULL,
    viewed_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX staff_access_views_access ON staff_access_views (access_id, viewed_at);

CREATE FUNCTION staff_access_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is append-only: its rows can''t be changed or removed', TG_TABLE_NAME
        USING ERRCODE = 'insufficient_privilege';
END
$$;

-- Emptying a table that holds nothing removes nothing, so a TRUNCATE is
-- refused only when there is something to lose: the development seed's
-- reset, which empties a database it made, cascades to these tables.
CREATE FUNCTION staff_access_keep_rows() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    any_rows boolean;
BEGIN
    EXECUTE format('SELECT EXISTS (SELECT 1 FROM %I)', TG_TABLE_NAME) INTO any_rows;
    IF any_rows THEN
        RAISE EXCEPTION '% is append-only: its rows can''t be changed or removed', TG_TABLE_NAME
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER staff_access_append_only
    BEFORE UPDATE OR DELETE ON staff_access
    FOR EACH ROW EXECUTE FUNCTION staff_access_append_only();
CREATE TRIGGER staff_access_no_truncate
    BEFORE TRUNCATE ON staff_access
    FOR EACH STATEMENT EXECUTE FUNCTION staff_access_keep_rows();
CREATE TRIGGER staff_access_views_append_only
    BEFORE UPDATE OR DELETE ON staff_access_views
    FOR EACH ROW EXECUTE FUNCTION staff_access_append_only();
CREATE TRIGGER staff_access_views_no_truncate
    BEFORE TRUNCATE ON staff_access_views
    FOR EACH STATEMENT EXECUTE FUNCTION staff_access_keep_rows();

-- The application connects as the role that runs the migrations, which
-- owns the tables; it keeps SELECT and INSERT and nothing else.
REVOKE UPDATE, DELETE, TRUNCATE ON staff_access, staff_access_views FROM PUBLIC;
REVOKE UPDATE, DELETE, TRUNCATE ON staff_access, staff_access_views FROM CURRENT_USER;

-- The grants a member holds now, oldest first, only while they are staff,
-- so that a moderator who is made a member again loses them at once:
-- their ids, or the thread or space each opens (NULL where it opens the
-- other), in the same order.
CREATE FUNCTION staff_access_now(member bigint, wanted text) RETURNS bigint[] LANGUAGE sql STABLE AS $$
    SELECT array(SELECT CASE wanted WHEN 'thread' THEN a.thread_id
                                    WHEN 'space' THEN a.space_id
                                    ELSE a.id END
                 FROM staff_access a JOIN users u ON u.id = a.actor_id
                 WHERE a.actor_id = member AND a.expires_at > now()
                   AND u.role IN ('moderator', 'admin')
                 ORDER BY a.id)
$$;

-- Staff read restricted spaces only where a read grant lets them in: by
-- naming them, a group or list of theirs, or "staff" (DESIGN.md §3.1).
-- That part of the view was already by grant and stays. It gains the
-- grants they hold now (milestone 15).
DROP VIEW member_contexts;
CREATE VIEW member_contexts AS
SELECT u.id, u.username, u.display_name, u.email,
       u.email_verified_at IS NOT NULL AS verified,
       u.role, u.created_at, u.silenced_until, u.suspended_until,
       member_group_ids(u.id) AS group_ids,
       array(SELECT g.space_id FROM space_grants g
             WHERE g.verb = 'read'
               AND ((g.level = 'listed'
                     AND (g.user_ids @> ARRAY[u.id] OR g.group_ids && member_group_ids(u.id)))
                    OR (g.level = 'staff' AND u.role IN ('moderator', 'admin')))
             ORDER BY 1) AS restricted_space_ids,
       array(SELECT b.blocked_user_id FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
             WHERE b.user_id = u.id AND bu.role = 'user' ORDER BY 1) AS blocked_user_ids,
       staff_access_now(u.id, 'access') AS access_ids,
       staff_access_now(u.id, 'thread') AS access_thread_ids,
       staff_access_now(u.id, 'space') AS access_space_ids
FROM users u
WHERE u.deleted_at IS NULL;

-- A conversation is read by the people in it, and by nobody else: a
-- report no longer opens it to staff (a staff member who opens a reported
-- conversation breaks glass on it instead, DESIGN.md §3.9, §8).
-- (Suspended members are given no viewer id here, so they read none.)
CREATE OR REPLACE FUNCTION dm_readable(thread_id bigint, visibility text, viewer_id bigint)
RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT visibility = 'private' AND viewer_id IS NOT NULL AND EXISTS (
        SELECT 1 FROM private_thread_participants p
        WHERE p.thread_id = dm_readable.thread_id AND p.user_id = viewer_id AND p.left_at IS NULL)
$$;

-- The one way to read under a grant (DESIGN.md §6): whether the grants a
-- staff member holds cover a thread (or conversation), by its id or its
-- space's. `thread_readable` itself never looks at grants or at roles;
-- the content functions that serve a page under a grant ask this as well,
-- and record a view whenever it is what let the viewer in.
CREATE FUNCTION thread_granted(t threads, granted_threads bigint[], granted_spaces bigint[])
RETURNS boolean LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT coalesce(t.id = ANY (granted_threads), false)
        OR coalesce(t.space_id = ANY (granted_spaces), false)
$$;
