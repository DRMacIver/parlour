-- Three corrections to 0029 (per-thread permissions, milestone 14).
--
-- A feed's owner holds every permission on it (DESIGN.md §2, §3.2;
-- migration 0028 for its grants), so a thread restriction in a feed names
-- its owner too, as it names the thread's author: whoever restricts a
-- thread there, and however a restricted thread comes to be there (it is
-- started there, restricted there or moved in), the owner can read it and
-- reply in it. Since a thread can be moved into a feed by staff, this adds
-- the owner rather than refusing; a thread moved out again keeps the names
-- it had.
--
-- The groups and lists a restriction names must exist, as a foreign key
-- would have them: the ids are checked, and the groups locked against
-- deletion, whenever a restriction changes.
--
-- Only live threads keep a group or list from being deleted. Nobody can
-- change a deleted thread's restriction, so a deleted thread naming a
-- list would keep it for good; instead, deleting a group takes its id out
-- of deleted threads' restrictions, which only narrows them, so a thread
-- restored later is never wider than it was.

CREATE OR REPLACE FUNCTION threads_restriction_names() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    owner bigint;
    named bigint[];
    found bigint;
BEGIN
    IF NEW.space_id IS NOT NULL THEN
        SELECT owner_user_id INTO owner FROM spaces WHERE id = NEW.space_id AND kind = 'feed';
    END IF;
    IF owner IS NOT NULL THEN
        IF NEW.read_user_ids IS NOT NULL AND NOT NEW.read_user_ids @> ARRAY[owner] THEN
            NEW.read_user_ids := NEW.read_user_ids || owner;
        END IF;
        IF NEW.reply_user_ids IS NOT NULL AND NOT NEW.reply_user_ids @> ARRAY[owner] THEN
            NEW.reply_user_ids := NEW.reply_user_ids || owner;
        END IF;
    END IF;
    named := ARRAY(SELECT DISTINCT g FROM unnest(coalesce(NEW.read_group_ids, '{}')
                                                 || coalesce(NEW.reply_group_ids, '{}')) g);
    IF cardinality(named) > 0
       AND (TG_OP = 'INSERT'
            OR OLD.read_group_ids IS DISTINCT FROM NEW.read_group_ids
            OR OLD.reply_group_ids IS DISTINCT FROM NEW.reply_group_ids) THEN
        SELECT count(*) INTO found
        FROM (SELECT id FROM groups WHERE id = ANY (named) FOR KEY SHARE) g;
        IF found <> cardinality(named) THEN
            RAISE EXCEPTION 'thread % names a group that doesn''t exist', NEW.id
                USING ERRCODE = 'foreign_key_violation';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER threads_restriction_names
    BEFORE INSERT OR UPDATE OF space_id, read_group_ids, read_user_ids, reply_group_ids, reply_user_ids
    ON threads
    FOR EACH ROW EXECUTE FUNCTION threads_restriction_names();

-- Mend any restriction saved before this in a feed that left its owner out.
UPDATE threads t
SET read_user_ids = CASE WHEN t.read_user_ids IS NULL OR t.read_user_ids @> ARRAY[c.owner_user_id]
                         THEN t.read_user_ids ELSE t.read_user_ids || c.owner_user_id END,
    reply_user_ids = CASE WHEN t.reply_user_ids IS NULL OR t.reply_user_ids @> ARRAY[c.owner_user_id]
                          THEN t.reply_user_ids ELSE t.reply_user_ids || c.owner_user_id END
FROM spaces c
WHERE c.id = t.space_id AND c.kind = 'feed'
  AND ((t.read_user_ids IS NOT NULL AND NOT t.read_user_ids @> ARRAY[c.owner_user_id])
       OR (t.reply_user_ids IS NOT NULL AND NOT t.reply_user_ids @> ARRAY[c.owner_user_id]));

CREATE OR REPLACE FUNCTION groups_refuse_delete_in_grants() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM space_grants WHERE group_ids @> ARRAY[OLD.id]) THEN
        RAISE EXCEPTION 'group % is named in a space''s grants', OLD.id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF EXISTS (SELECT 1 FROM threads
               WHERE deleted_at IS NULL
                 AND (read_group_ids @> ARRAY[OLD.id] OR reply_group_ids @> ARRAY[OLD.id])) THEN
        RAISE EXCEPTION 'group % is named in a thread''s restriction', OLD.id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    UPDATE threads
    SET read_group_ids = array_remove(read_group_ids, OLD.id),
        reply_group_ids = array_remove(reply_group_ids, OLD.id)
    WHERE deleted_at IS NOT NULL
      AND (read_group_ids @> ARRAY[OLD.id] OR reply_group_ids @> ARRAY[OLD.id]);
    RETURN OLD;
END;
$$;
