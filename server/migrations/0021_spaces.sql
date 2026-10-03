-- Categories become spaces (DESIGN.md v0.2 §3.1). This migration only
-- renames: every table, column, index, function and view that said
-- "category" now says "space", and so do the strings stored in
-- notification kinds and moderation-log details. Who may read and post
-- is still decided by the old read and write levels; the next migration
-- replaces those with grants.
--
-- Postgres follows renamed tables and columns in constraints, views and
-- triggers, but not in the text of SQL and plpgsql function bodies, so
-- every function whose body named a category is defined again here.

ALTER TABLE categories RENAME TO spaces;
ALTER INDEX categories_section_position RENAME TO spaces_section_position;
ALTER INDEX categories_pkey RENAME TO spaces_pkey;
ALTER INDEX categories_slug_key RENAME TO spaces_slug_key;
ALTER TABLE spaces RENAME CONSTRAINT categories_read_level_check TO spaces_read_level_check;
ALTER TABLE spaces RENAME CONSTRAINT categories_write_level_check TO spaces_write_level_check;
ALTER TABLE spaces RENAME CONSTRAINT categories_slug_check TO spaces_slug_check;

ALTER TABLE category_groups RENAME TO space_groups;
ALTER TABLE space_groups RENAME COLUMN category_id TO space_id;
ALTER INDEX category_groups_group RENAME TO space_groups_group;
ALTER INDEX category_groups_pkey RENAME TO space_groups_pkey;

ALTER TABLE user_category_post_counts RENAME TO user_space_post_counts;
ALTER TABLE user_space_post_counts RENAME COLUMN category_id TO space_id;
ALTER INDEX user_category_post_counts_pkey RENAME TO user_space_post_counts_pkey;

ALTER TABLE threads RENAME COLUMN category_id TO space_id;
ALTER INDEX threads_category_listing RENAME TO threads_space_listing;

ALTER TABLE watches RENAME COLUMN category_id TO space_id;
ALTER INDEX watches_category RENAME TO watches_space;
ALTER INDEX watches_category_level RENAME TO watches_space_level;

ALTER TABLE held_posts RENAME COLUMN category_id TO space_id;

-- Stored strings.
ALTER TABLE notifications DROP CONSTRAINT notifications_kind_check;
UPDATE notifications SET kind = 'watched_space' WHERE kind = 'watched_category';
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check
    CHECK (kind IN ('reply', 'mention', 'message', 'watched_thread', 'watched_space', 'moderation'));

UPDATE mod_actions
SET details = (details - 'from_category_id' - 'to_category_id')
              || CASE WHEN details ? 'from_category_id'
                      THEN jsonb_build_object('from_space_id', details->'from_category_id') ELSE '{}' END
              || CASE WHEN details ? 'to_category_id'
                      THEN jsonb_build_object('to_space_id', details->'to_category_id') ELSE '{}' END
WHERE details ? 'from_category_id' OR details ? 'to_category_id';

-- Functions and triggers whose bodies named categories.

CREATE OR REPLACE FUNCTION threads_set_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.space_id IS NULL THEN
        NEW.visibility := 'private';
    ELSE
        SELECT read_level INTO NEW.visibility FROM spaces WHERE id = NEW.space_id FOR SHARE;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER categories_propagate_visibility ON spaces;
DROP FUNCTION categories_propagate_visibility();
CREATE FUNCTION spaces_propagate_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE threads SET visibility = NEW.read_level WHERE space_id = NEW.id;
    RETURN NULL;
END;
$$;
CREATE TRIGGER spaces_propagate_visibility AFTER UPDATE OF read_level ON spaces
    FOR EACH ROW WHEN (OLD.read_level IS DISTINCT FROM NEW.read_level)
    EXECUTE FUNCTION spaces_propagate_visibility();

-- The one visibility condition (DESIGN.md §6): `vis` lists the visibility
-- levels the viewer can read everywhere, and `group_space_ids` the
-- group-restricted spaces they can read.
DROP FUNCTION thread_readable(text, bigint, text[], bigint[]);
CREATE FUNCTION thread_readable(visibility text, space_id bigint, vis text[], group_space_ids bigint[])
RETURNS boolean LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT visibility = ANY (vis) OR (visibility = 'groups' AND space_id = ANY (group_space_ids))
$$;

DROP FUNCTION category_readable(text, bigint, text[], bigint[]);
CREATE FUNCTION space_readable(read_level text, space_id bigint, vis text[], group_space_ids bigint[])
RETURNS boolean LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT read_level = ANY (vis) OR (read_level = 'groups' AND space_id = ANY (group_space_ids))
$$;

-- Post counts per member and space (migration 0019).
DROP FUNCTION adjust_post_count(bigint, bigint, bigint);
CREATE FUNCTION adjust_post_count(member bigint, space bigint, delta bigint)
RETURNS void LANGUAGE sql AS $$
    INSERT INTO user_space_post_counts (user_id, space_id, posts)
    VALUES (member, space, delta)
    ON CONFLICT (user_id, space_id)
    DO UPDATE SET posts = user_space_post_counts.posts + EXCLUDED.posts
$$;

CREATE OR REPLACE FUNCTION post_counted_in(thread bigint, deleted timestamptz, hidden timestamptz)
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT t.space_id FROM threads t
    WHERE t.id = thread AND t.deleted_at IS NULL AND deleted IS NULL AND hidden IS NULL
$$;

CREATE OR REPLACE FUNCTION count_post_changes() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    space bigint;
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        space := post_counted_in(OLD.thread_id, OLD.deleted_at, OLD.hidden_at);
        IF space IS NOT NULL THEN
            PERFORM adjust_post_count(OLD.author_id, space, -1);
        END IF;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        space := post_counted_in(NEW.thread_id, NEW.deleted_at, NEW.hidden_at);
        IF space IS NOT NULL THEN
            PERFORM adjust_post_count(NEW.author_id, space, 1);
        END IF;
    END IF;
    RETURN NULL;
END
$$;

CREATE OR REPLACE FUNCTION count_thread_changes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.space_id IS NOT NULL AND OLD.deleted_at IS NULL THEN
        INSERT INTO user_space_post_counts (user_id, space_id, posts)
        SELECT p.author_id, OLD.space_id, -count(*)
        FROM posts p
        WHERE p.thread_id = OLD.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
        GROUP BY p.author_id
        ON CONFLICT (user_id, space_id)
        DO UPDATE SET posts = user_space_post_counts.posts + EXCLUDED.posts;
    END IF;
    IF NEW.space_id IS NOT NULL AND NEW.deleted_at IS NULL THEN
        INSERT INTO user_space_post_counts (user_id, space_id, posts)
        SELECT p.author_id, NEW.space_id, count(*)
        FROM posts p
        WHERE p.thread_id = NEW.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
        GROUP BY p.author_id
        ON CONFLICT (user_id, space_id)
        DO UPDATE SET posts = user_space_post_counts.posts + EXCLUDED.posts;
    END IF;
    RETURN NULL;
END
$$;

-- The shared definitions from migration 0020.
DROP VIEW member_contexts;
CREATE VIEW member_contexts AS
SELECT u.id, u.username, u.display_name, u.email,
       u.email_verified_at IS NOT NULL AS verified,
       u.role, u.created_at, u.silenced_until, u.suspended_until,
       array(SELECT m.group_id FROM group_members m WHERE m.user_id = u.id ORDER BY 1) AS group_ids,
       array(SELECT DISTINCT sg.space_id FROM space_groups sg
             JOIN group_members m ON m.group_id = sg.group_id
             WHERE m.user_id = u.id AND sg.access = 'read' ORDER BY 1) AS group_space_ids,
       array(SELECT b.blocked_user_id FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
             WHERE b.user_id = u.id AND bu.role = 'user' ORDER BY 1) AS blocked_user_ids
FROM users u
WHERE u.deleted_at IS NULL;

DROP VIEW category_targets;
CREATE VIEW space_targets AS
SELECT c.id, c.slug, c.name, c.description, c.position, c.section_id,
       s.name AS section_name, s.position AS section_position,
       c.read_level, c.write_level,
       array(SELECT sg.group_id FROM space_groups sg
             WHERE sg.space_id = c.id AND sg.access = 'read' ORDER BY 1) AS read_groups,
       array(SELECT sg.group_id FROM space_groups sg
             WHERE sg.space_id = c.id AND sg.access = 'write' ORDER BY 1) AS write_groups,
       array(SELECT g.name FROM space_groups sg JOIN groups g ON g.id = sg.group_id
             WHERE sg.space_id = c.id AND sg.access = 'write' AND c.write_level = 'groups'
             ORDER BY lower(g.name), g.id) AS write_group_names,
       array(SELECT g.name FROM space_groups sg JOIN groups g ON g.id = sg.group_id
             WHERE sg.space_id = c.id AND sg.access = 'read' AND c.read_level = 'groups'
             ORDER BY lower(g.name), g.id) AS group_names
FROM spaces c LEFT JOIN sections s ON s.id = c.section_id;

DROP FUNCTION unread_notification_count(bigint, text[], bigint[], bigint);
CREATE FUNCTION unread_notification_count(member bigint, vis text[], group_space_ids bigint[], dm_viewer bigint)
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT count(*)
    FROM notifications n
    LEFT JOIN posts p ON p.id = n.post_id
    LEFT JOIN threads t ON t.id = p.thread_id
    WHERE n.user_id = member AND n.read_at IS NULL
      AND (n.post_id IS NULL
           OR (p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
               AND (thread_readable(t.visibility, t.space_id, vis, group_space_ids)
                    OR dm_readable(t.id, t.visibility, dm_viewer))))
      AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                      WHERE b.user_id = member AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
$$;
