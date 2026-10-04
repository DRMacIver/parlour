-- Two corrections to 0027 (feeds, milestone 13).
--
-- The owner holds every permission on their feed (DESIGN.md §2, §3.2),
-- not only `administer`: whatever its grants say, they can read it,
-- reply and start threads in it, and run it, so they can never lock
-- themselves out. Each of a feed's grants must let every member (or
-- everyone) through, or name the owner. The application names them; this
-- makes sure, and mends any grant saved before it that left them out
-- (one that named people gets them added; "only moderators and admins"
-- becomes the owner alone).
--
-- 0027 told the people who ran a topic space it moved aside, and the
-- admins, naming the space, without asking whether they could read it.
-- The notice is kept only for those who can, as the access layer decides
-- it (staff still read every space until milestone 15; a suspension is
-- passing, so it isn't counted); anyone else loses it.

CREATE OR REPLACE FUNCTION space_grants_feed_owner() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    owner bigint;
BEGIN
    SELECT owner_user_id INTO owner FROM spaces WHERE id = NEW.space_id AND kind = 'feed';
    IF owner IS NOT NULL
       AND NOT (NEW.level IN ('public', 'members')
                OR (NEW.level = 'listed' AND NEW.user_ids @> ARRAY[owner])) THEN
        RAISE EXCEPTION 'the owner of feed % holds every permission on it', NEW.space_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

UPDATE space_grants g
SET level = 'listed',
    user_ids = CASE WHEN g.level = 'listed' THEN g.user_ids || c.owner_user_id
                    ELSE ARRAY[c.owner_user_id] END,
    group_ids = CASE WHEN g.level = 'listed' THEN g.group_ids ELSE '{}' END
FROM spaces c
WHERE c.id = g.space_id AND c.kind = 'feed'
  AND NOT (g.level IN ('public', 'members')
           OR (g.level = 'listed' AND g.user_ids @> ARRAY[c.owner_user_id]));

DELETE FROM notifications n
WHERE n.kind = 'moderation'
  AND n.message LIKE 'Every member now has a space of their own, a feed, at their username, so the space “%'
  AND NOT EXISTS (
      SELECT 1
      FROM spaces c, member_contexts m
      WHERE m.id = n.user_id
        AND c.kind = 'topic'
        AND c.slug = substring(n.message from '^.* to /c/([a-z0-9-]+), since [^ ]+ is a member’s username\.')
        AND space_readable(c.visibility, c.id,
                           CASE WHEN m.role IN ('moderator', 'admin')
                                THEN ARRAY['public', 'members', 'restricted']
                                ELSE ARRAY['public', 'members'] END,
                           m.restricted_space_ids));
