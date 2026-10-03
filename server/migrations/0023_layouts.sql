-- Layouts (DESIGN.md v0.2 §3.5, §4): the front page is an ordered list of
-- named sections, each an ordered list of spaces. The row of `layouts`
-- with no user is the admin-set default; every member has a row of their
-- own, copied from the default when their account is made and theirs to
-- edit from then on. A member who hasn't edited theirs keeps following
-- the default: changing it changes their copy too.
--
-- The old display-only `sections` become the default layout, one section
-- per old section (preceded by a "Spaces" section for spaces that were
-- outside any section, if there were any), and every member gets a copy
-- with an empty "Feeds" section at the end, where their own feed goes
-- when feeds arrive (milestone 13). Then the old table and columns go.
--
-- Admins can also mark spaces essential: shown at the top of every front
-- page whatever the layout, and never muted.

ALTER TABLE spaces ADD COLUMN essential boolean NOT NULL DEFAULT false;

CREATE TABLE layouts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id bigint UNIQUE REFERENCES users (id) ON DELETE CASCADE,
    edited_at timestamptz
);

-- There is exactly one default. The expression index lets an insert say
-- ON CONFLICT, so that making the default on demand is safe to race.
CREATE UNIQUE INDEX layouts_one_default ON layouts ((user_id IS NULL)) WHERE user_id IS NULL;

CREATE TABLE layout_sections (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    layout_id bigint NOT NULL REFERENCES layouts (id) ON DELETE CASCADE,
    name text NOT NULL,
    position integer NOT NULL DEFAULT 0
);

CREATE INDEX layout_sections_layout ON layout_sections (layout_id, position, id);

CREATE TABLE layout_section_spaces (
    section_id bigint NOT NULL REFERENCES layout_sections (id) ON DELETE CASCADE,
    space_id bigint NOT NULL REFERENCES spaces (id) ON DELETE CASCADE,
    position integer NOT NULL DEFAULT 0,
    PRIMARY KEY (section_id, space_id)
);

CREATE INDEX layout_section_spaces_space ON layout_section_spaces (space_id);

-- A space is in at most one section of a layout. The application moves a
-- space between sections by deleting and inserting, so only inserts need
-- checking.
CREATE FUNCTION layout_section_spaces_once() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM layout_section_spaces x
        JOIN layout_sections s ON s.id = x.section_id
        JOIN layout_sections n ON n.id = NEW.section_id
        WHERE x.space_id = NEW.space_id AND x.section_id <> NEW.section_id AND s.layout_id = n.layout_id
    ) THEN
        RAISE EXCEPTION 'space % is already in a section of this layout', NEW.space_id
            USING ERRCODE = 'unique_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER layout_section_spaces_once BEFORE INSERT ON layout_section_spaces
    FOR EACH ROW EXECUTE FUNCTION layout_section_spaces_once();

CREATE TABLE space_mutes (
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    space_id bigint NOT NULL REFERENCES spaces (id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, space_id)
);

-- Essential spaces can't be muted (DESIGN.md §3.5). The application
-- refuses first, and this makes sure.
CREATE FUNCTION space_mutes_refuse_essential() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM spaces WHERE id = NEW.space_id AND essential) THEN
        RAISE EXCEPTION 'space % is essential and cannot be muted', NEW.space_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER space_mutes_refuse_essential BEFORE INSERT OR UPDATE ON space_mutes
    FOR EACH ROW EXECUTE FUNCTION space_mutes_refuse_essential();

-- Makes `target` a copy of the default layout, returning `target`. The
-- member's own "Feeds" section is theirs and is kept, with what is in it,
-- at the end (made empty if they had none); anything the default's
-- "Feeds" section holds joins it. Used when an account is made, and
-- again for every member who hasn't edited their copy whenever the
-- default changes (`layout_propagate_default`).
CREATE FUNCTION layout_copy_default(target bigint) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    d bigint;
    feeds bigint;
    s record;
    sec bigint;
BEGIN
    -- One writer at a time per layout: a member editing theirs waits for
    -- a copy in progress, and the other way round.
    PERFORM id FROM layouts WHERE id = target FOR UPDATE;
    SELECT id INTO d FROM layouts WHERE user_id IS NULL;
    SELECT id INTO feeds FROM layout_sections
    WHERE layout_id = target AND name = 'Feeds'
    ORDER BY position, id LIMIT 1;
    DELETE FROM layout_sections WHERE layout_id = target AND id IS DISTINCT FROM feeds;
    IF d IS NOT NULL THEN
        FOR s IN SELECT id, name, position FROM layout_sections
                 WHERE layout_id = d AND name <> 'Feeds' ORDER BY position, id
        LOOP
            INSERT INTO layout_sections (layout_id, name, position)
            VALUES (target, s.name, s.position) RETURNING id INTO sec;
            INSERT INTO layout_section_spaces (section_id, space_id, position)
            SELECT sec, x.space_id, x.position
            FROM layout_section_spaces x
            WHERE x.section_id = s.id
              AND NOT EXISTS (SELECT 1 FROM layout_section_spaces f
                              WHERE f.section_id = feeds AND f.space_id = x.space_id)
            ORDER BY x.position, x.space_id;
        END LOOP;
    END IF;
    IF feeds IS NULL THEN
        INSERT INTO layout_sections (layout_id, name, position)
        SELECT target, 'Feeds', coalesce(max(position), -1) + 1
        FROM layout_sections WHERE layout_id = target
        RETURNING id INTO feeds;
    ELSE
        UPDATE layout_sections SET position = (SELECT coalesce(max(position), -1) + 1
                                               FROM layout_sections
                                               WHERE layout_id = target AND id <> feeds)
        WHERE id = feeds;
    END IF;
    IF d IS NOT NULL THEN
        INSERT INTO layout_section_spaces (section_id, space_id, position)
        SELECT feeds, x.space_id,
               (SELECT coalesce(max(position), -1) FROM layout_section_spaces WHERE section_id = feeds)
                   + row_number() OVER (ORDER BY ds.position, ds.id, x.position, x.space_id)
        FROM layout_sections ds
        JOIN layout_section_spaces x ON x.section_id = ds.id
        WHERE ds.layout_id = d AND ds.name = 'Feeds'
          AND NOT EXISTS (SELECT 1 FROM layout_section_spaces y
                          JOIN layout_sections ys ON ys.id = y.section_id
                          WHERE ys.layout_id = target AND y.space_id = x.space_id);
    END IF;
    RETURN target;
END;
$$;

-- Copies the default again into every layout that still follows it,
-- returning how many.
CREATE FUNCTION layout_propagate_default() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
    n integer := 0;
    l bigint;
BEGIN
    FOR l IN SELECT id FROM layouts WHERE user_id IS NOT NULL AND edited_at IS NULL ORDER BY id LOOP
        PERFORM layout_copy_default(l);
        n := n + 1;
    END LOOP;
    RETURN n;
END;
$$;

-- The default layout, from the old sections, and a copy for every member.
DO $$
DECLARE
    d bigint;
    s record;
    sec bigint;
    n integer := 0;
    member bigint;
    copy bigint;
BEGIN
    INSERT INTO layouts (user_id) VALUES (NULL) RETURNING id INTO d;
    -- Spaces outside any section came first on the old index, under the
    -- heading "Spaces".
    IF EXISTS (SELECT 1 FROM spaces WHERE section_id IS NULL) THEN
        INSERT INTO layout_sections (layout_id, name, position) VALUES (d, 'Spaces', n) RETURNING id INTO sec;
        INSERT INTO layout_section_spaces (section_id, space_id, position)
        SELECT sec, id, row_number() OVER (ORDER BY position, id) - 1 FROM spaces WHERE section_id IS NULL;
        n := n + 1;
    END IF;
    FOR s IN SELECT id, name FROM sections ORDER BY position, id LOOP
        INSERT INTO layout_sections (layout_id, name, position) VALUES (d, s.name, n) RETURNING id INTO sec;
        INSERT INTO layout_section_spaces (section_id, space_id, position)
        SELECT sec, id, row_number() OVER (ORDER BY position, id) - 1 FROM spaces WHERE section_id = s.id;
        n := n + 1;
    END LOOP;
    FOR member IN SELECT id FROM users WHERE deleted_at IS NULL ORDER BY id LOOP
        INSERT INTO layouts (user_id) VALUES (member) RETURNING id INTO copy;
        PERFORM layout_copy_default(copy);
    END LOOP;
END;
$$;

-- The space, as writers see it: no longer placed in a section of its own,
-- but possibly essential.
DROP VIEW space_targets;
CREATE VIEW space_targets AS
SELECT c.id, c.slug, c.name, c.description, c.essential,
       c.visibility, c.created_by, c.created_at, c.archived_at,
       r.level AS read_level, r.group_ids AS read_group_ids, r.user_ids AS read_user_ids,
       p.level AS reply_level, p.group_ids AS reply_group_ids, p.user_ids AS reply_user_ids,
       st.level AS start_level, st.group_ids AS start_group_ids, st.user_ids AS start_user_ids,
       a.level AS administer_level, a.group_ids AS administer_group_ids, a.user_ids AS administer_user_ids,
       array(SELECT g.name FROM groups g WHERE g.id = ANY (r.group_ids)
             ORDER BY lower(g.name), g.id) AS read_group_names
FROM spaces c
JOIN space_grants r ON r.space_id = c.id AND r.verb = 'read'
JOIN space_grants p ON p.space_id = c.id AND p.verb = 'reply'
JOIN space_grants st ON st.space_id = c.id AND st.verb = 'start'
JOIN space_grants a ON a.space_id = c.id AND a.verb = 'administer';

ALTER TABLE spaces DROP COLUMN section_id, DROP COLUMN position;
DROP TABLE sections;
