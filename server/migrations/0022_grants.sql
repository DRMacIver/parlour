-- Grants (DESIGN.md v0.2 §3.1): who may read, reply, start threads and
-- administer each space, one row per verb, replacing the read and write
-- levels and `space_groups`. A grant is a level (`public`, only for
-- reading; `members`; `staff`; or `listed`) and, for `listed`, the groups
-- and people named.
--
-- `spaces.visibility` mirrors the read grant (`public`, `members` or
-- `restricted`) the way `threads.visibility` mirrors its space's, so the
-- access layer keeps its one-line condition; the old `groups` visibility
-- is now called `restricted`.

CREATE TABLE space_grants (
    space_id bigint NOT NULL REFERENCES spaces (id) ON DELETE CASCADE,
    verb text NOT NULL CHECK (verb IN ('read', 'reply', 'start', 'administer')),
    level text NOT NULL CHECK (level IN ('public', 'members', 'staff', 'listed')),
    group_ids bigint[] NOT NULL DEFAULT '{}',
    user_ids bigint[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (space_id, verb),
    CHECK (verb = 'read' OR level <> 'public'),
    CHECK (level = 'listed' OR (group_ids = '{}' AND user_ids = '{}'))
);

CREATE INDEX space_grants_groups ON space_grants USING gin (group_ids);
CREATE INDEX space_grants_users ON space_grants USING gin (user_ids);

-- The old levels, as grants. A `groups` level that named no groups let
-- only staff through, so it becomes `staff`.
INSERT INTO space_grants (space_id, verb, level, group_ids)
SELECT s.id, 'read',
       CASE s.read_level WHEN 'groups' THEN CASE WHEN r.ids = '{}' THEN 'staff' ELSE 'listed' END
                         ELSE s.read_level END,
       CASE WHEN s.read_level = 'groups' THEN r.ids ELSE '{}' END
FROM spaces s
CROSS JOIN LATERAL (SELECT coalesce(array_agg(group_id ORDER BY group_id), '{}') AS ids
                    FROM space_groups WHERE space_id = s.id AND access = 'read') r;

INSERT INTO space_grants (space_id, verb, level, group_ids)
SELECT s.id, v.verb,
       CASE s.write_level WHEN 'groups' THEN CASE WHEN w.ids = '{}' THEN 'staff' ELSE 'listed' END
                          ELSE s.write_level END,
       CASE WHEN s.write_level = 'groups' THEN w.ids ELSE '{}' END
FROM spaces s
CROSS JOIN (VALUES ('reply'), ('start')) v (verb)
CROSS JOIN LATERAL (SELECT coalesce(array_agg(group_id ORDER BY group_id), '{}') AS ids
                    FROM space_groups WHERE space_id = s.id AND access = 'write') w;

-- Every space so far was made by an admin; staff administer everything
-- anyway, so nobody else is named.
INSERT INTO space_grants (space_id, verb, level)
SELECT id, 'administer', 'staff' FROM spaces;

ALTER TABLE spaces
    ADD COLUMN created_by bigint REFERENCES users (id),
    ADD COLUMN created_at timestamptz NOT NULL DEFAULT now(),
    ADD COLUMN archived_at timestamptz,
    ADD COLUMN visibility text;

UPDATE spaces s
SET visibility = CASE g.level WHEN 'public' THEN 'public' WHEN 'members' THEN 'members' ELSE 'restricted' END
FROM space_grants g
WHERE g.space_id = s.id AND g.verb = 'read';

ALTER TABLE spaces
    ALTER COLUMN visibility SET NOT NULL,
    ADD CONSTRAINT spaces_visibility_check CHECK (visibility IN ('public', 'members', 'restricted'));

-- spaces.visibility always mirrors the read grant. A space without one yet
-- (just inserted) is restricted until its grant arrives.
CREATE FUNCTION space_visibility_of(level text) RETURNS text LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE level WHEN 'public' THEN 'public' WHEN 'members' THEN 'members' ELSE 'restricted' END
$$;

CREATE FUNCTION spaces_set_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    SELECT space_visibility_of(level) INTO NEW.visibility
    FROM space_grants WHERE space_id = NEW.id AND verb = 'read';
    IF NOT FOUND THEN
        NEW.visibility := 'restricted';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER spaces_set_visibility BEFORE INSERT OR UPDATE OF visibility ON spaces
    FOR EACH ROW EXECUTE FUNCTION spaces_set_visibility();

CREATE FUNCTION space_grants_set_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.verb = 'read' THEN
        UPDATE spaces SET visibility = space_visibility_of(NEW.level) WHERE id = NEW.space_id;
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER space_grants_set_visibility AFTER INSERT OR UPDATE OF level ON space_grants
    FOR EACH ROW EXECUTE FUNCTION space_grants_set_visibility();

-- Threads follow their space's visibility, as before.
DROP TRIGGER spaces_propagate_visibility ON spaces;
DROP FUNCTION spaces_propagate_visibility();
CREATE FUNCTION spaces_propagate_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE threads SET visibility = NEW.visibility WHERE space_id = NEW.id;
    RETURN NULL;
END;
$$;
CREATE TRIGGER spaces_propagate_visibility AFTER UPDATE OF visibility ON spaces
    FOR EACH ROW WHEN (OLD.visibility IS DISTINCT FROM NEW.visibility)
    EXECUTE FUNCTION spaces_propagate_visibility();

-- Locking the space's row FOR SHARE serialises thread moves with changes to
-- its read grant (see migration 0003).
CREATE OR REPLACE FUNCTION threads_set_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.space_id IS NULL THEN
        NEW.visibility := 'private';
    ELSE
        SELECT visibility INTO NEW.visibility FROM spaces WHERE id = NEW.space_id FOR SHARE;
    END IF;
    RETURN NEW;
END;
$$;

ALTER TABLE threads DROP CONSTRAINT threads_visibility_check;
UPDATE threads SET visibility = 'restricted' WHERE visibility = 'groups';
ALTER TABLE threads ADD CONSTRAINT threads_visibility_check
    CHECK (visibility IN ('public', 'members', 'restricted', 'private'));

-- The one visibility condition: `vis` lists the visibility levels the
-- viewer reads everywhere, and `restricted_space_ids` the restricted
-- spaces they may read (those whose read grant names them or a group of
-- theirs, and, for staff, those for staff).
DROP FUNCTION thread_readable(text, bigint, text[], bigint[]);
CREATE FUNCTION thread_readable(visibility text, space_id bigint, vis text[], restricted_space_ids bigint[])
RETURNS boolean LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT visibility = ANY (vis) OR (visibility = 'restricted' AND space_id = ANY (restricted_space_ids))
$$;

DROP FUNCTION space_readable(text, bigint, text[], bigint[]);
CREATE FUNCTION space_readable(visibility text, space_id bigint, vis text[], restricted_space_ids bigint[])
RETURNS boolean LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT visibility = ANY (vis) OR (visibility = 'restricted' AND space_id = ANY (restricted_space_ids))
$$;

DROP FUNCTION unread_notification_count(bigint, text[], bigint[], bigint);
CREATE FUNCTION unread_notification_count(member bigint, vis text[], restricted_space_ids bigint[], dm_viewer bigint)
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT count(*)
    FROM notifications n
    LEFT JOIN posts p ON p.id = n.post_id
    LEFT JOIN threads t ON t.id = p.thread_id
    WHERE n.user_id = member AND n.read_at IS NULL
      AND (n.post_id IS NULL
           OR (p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
               AND (thread_readable(t.visibility, t.space_id, vis, restricted_space_ids)
                    OR dm_readable(t.id, t.visibility, dm_viewer))))
      AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                      WHERE b.user_id = member AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
$$;

DROP VIEW member_contexts;
CREATE VIEW member_contexts AS
SELECT u.id, u.username, u.display_name, u.email,
       u.email_verified_at IS NOT NULL AS verified,
       u.role, u.created_at, u.silenced_until, u.suspended_until,
       array(SELECT m.group_id FROM group_members m WHERE m.user_id = u.id ORDER BY 1) AS group_ids,
       array(SELECT g.space_id FROM space_grants g
             WHERE g.verb = 'read'
               AND ((g.level = 'listed'
                     AND (g.user_ids @> ARRAY[u.id]
                          OR g.group_ids && array(SELECT m.group_id FROM group_members m WHERE m.user_id = u.id)))
                    OR (g.level = 'staff' AND u.role IN ('moderator', 'admin')))
             ORDER BY 1) AS restricted_space_ids,
       array(SELECT b.blocked_user_id FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
             WHERE b.user_id = u.id AND bu.role = 'user' ORDER BY 1) AS blocked_user_ids
FROM users u
WHERE u.deleted_at IS NULL;

DROP VIEW space_targets;
CREATE VIEW space_targets AS
SELECT c.id, c.slug, c.name, c.description, c.position, c.section_id,
       s.name AS section_name, s.position AS section_position,
       c.visibility, c.created_by, c.created_at, c.archived_at,
       r.level AS read_level, r.group_ids AS read_group_ids, r.user_ids AS read_user_ids,
       p.level AS reply_level, p.group_ids AS reply_group_ids, p.user_ids AS reply_user_ids,
       st.level AS start_level, st.group_ids AS start_group_ids, st.user_ids AS start_user_ids,
       a.level AS administer_level, a.group_ids AS administer_group_ids, a.user_ids AS administer_user_ids,
       array(SELECT g.name FROM groups g WHERE g.id = ANY (r.group_ids)
             ORDER BY lower(g.name), g.id) AS read_group_names
FROM spaces c
LEFT JOIN sections s ON s.id = c.section_id
JOIN space_grants r ON r.space_id = c.id AND r.verb = 'read'
JOIN space_grants p ON p.space_id = c.id AND p.verb = 'reply'
JOIN space_grants st ON st.space_id = c.id AND st.verb = 'start'
JOIN space_grants a ON a.space_id = c.id AND a.verb = 'administer';

ALTER TABLE spaces DROP COLUMN read_level, DROP COLUMN write_level;
DROP TABLE space_groups;

-- A group named in a grant can't be deleted (DESIGN.md §3.3); the
-- application refuses first, and this makes sure.
CREATE FUNCTION groups_refuse_delete_in_grants() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM space_grants WHERE group_ids @> ARRAY[OLD.id]) THEN
        RAISE EXCEPTION 'group % is named in a space''s grants', OLD.id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    RETURN OLD;
END;
$$;

CREATE TRIGGER groups_refuse_delete_in_grants BEFORE DELETE ON groups
    FOR EACH ROW EXECUTE FUNCTION groups_refuse_delete_in_grants();

-- Moderators' actions on spaces (changing settings, archiving) are
-- recorded too (DESIGN.md §8).
ALTER TABLE mod_actions DROP CONSTRAINT mod_actions_target_kind_check;
ALTER TABLE mod_actions ADD CONSTRAINT mod_actions_target_kind_check
    CHECK (target_kind IN ('post', 'thread', 'user', 'revision', 'report', 'held_post', 'upload', 'space'));
