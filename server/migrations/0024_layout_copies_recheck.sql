-- Two corrections to how the default layout is copied into members'
-- layouts (migration 0023).
--
-- First, a copy skips the spaces the member has muted. Muting takes a
-- space out of the member's sections; a copy of the default that put it
-- back (hidden only by the front page's filter) would make unmuting mean
-- one thing for a member who follows the default and another for one who
-- has edited their layout. Unmuting never puts a space back in a section:
-- following it does.
--
-- Second, propagating a change to the default checks again, once it has a
-- member's row lock, that they still follow the default. The loop lists
-- the followers from a snapshot taken as it opens; a member's first edit
-- of their layout, committed while the propagation waits for their row,
-- would otherwise be copied over (and they would be left marked as
-- edited, so later changes to the default wouldn't reach them either).
-- `FOR UPDATE` re-evaluates the condition against the row as it is once
-- the lock is had, so the edit is seen and the layout left alone.

CREATE OR REPLACE FUNCTION layout_copy_default(target bigint) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    d bigint;
    owner bigint;
    feeds bigint;
    s record;
    sec bigint;
BEGIN
    -- One writer at a time per layout: a member editing theirs waits for
    -- a copy in progress, and the other way round.
    SELECT user_id INTO owner FROM layouts WHERE id = target FOR UPDATE;
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
              AND NOT EXISTS (SELECT 1 FROM space_mutes m
                              WHERE m.user_id = owner AND m.space_id = x.space_id)
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
                          WHERE ys.layout_id = target AND y.space_id = x.space_id)
          AND NOT EXISTS (SELECT 1 FROM space_mutes m
                          WHERE m.user_id = owner AND m.space_id = x.space_id);
    END IF;
    RETURN target;
END;
$$;

-- Copies the default again into every layout that still follows it,
-- returning how many. Each layout is checked again once its row is
-- locked: an edit committed meanwhile takes it out of the default's reach.
CREATE OR REPLACE FUNCTION layout_propagate_default() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
    n integer := 0;
    l bigint;
BEGIN
    FOR l IN SELECT id FROM layouts WHERE user_id IS NOT NULL AND edited_at IS NULL ORDER BY id LOOP
        PERFORM id FROM layouts WHERE id = l AND edited_at IS NULL FOR UPDATE;
        IF FOUND THEN
            PERFORM layout_copy_default(l);
            n := n + 1;
        END IF;
    END LOOP;
    RETURN n;
END;
$$;
