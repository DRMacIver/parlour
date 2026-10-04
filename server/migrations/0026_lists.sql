-- Lists (DESIGN.md §3.3, §4; milestone 12): groups that a member owns.
-- A group with an owner is a list; one without is a site group, made and
-- run by admins as before. Only a list's owner adds and removes its
-- members, and a list is hidden: only its owner and the people on it know
-- it exists. Every member has a list called "Friends" from the moment
-- their account exists. A list's owner counts as on it wherever a grant
-- names it, without being one of its members.
--
-- When the owner's account is deleted the list is frozen: its membership
-- can't change any more, and grants naming it keep working.

ALTER TABLE groups
    ADD COLUMN owner_user_id bigint REFERENCES users (id),
    ADD COLUMN frozen_at timestamptz,
    ALTER COLUMN slug DROP NOT NULL,
    -- Site groups have a short name for admins; lists don't need one.
    ADD CONSTRAINT groups_slug_for_site_groups CHECK ((owner_user_id IS NULL) = (slug IS NOT NULL)),
    ADD CONSTRAINT groups_frozen_lists CHECK (frozen_at IS NULL OR owner_user_id IS NOT NULL);

-- Site groups' names label spaces, so two can't share one; a member's
-- lists can't share one either, but every member has a "Friends".
DROP INDEX groups_name;
CREATE UNIQUE INDEX groups_name ON groups (lower(name)) WHERE owner_user_id IS NULL;
CREATE UNIQUE INDEX groups_list_name ON groups (owner_user_id, lower(name)) WHERE owner_user_id IS NOT NULL;

-- Every member has a Friends list, made with the account wherever
-- accounts are made.
INSERT INTO groups (name, owner_user_id)
SELECT 'Friends', id FROM users WHERE deleted_at IS NULL ORDER BY id;

CREATE FUNCTION users_make_friends_list() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO groups (name, owner_user_id) VALUES ('Friends', NEW.id);
    RETURN NULL;
END;
$$;

CREATE TRIGGER users_make_friends_list AFTER INSERT ON users
    FOR EACH ROW EXECUTE FUNCTION users_make_friends_list();

-- Deleting an account (by the member, or as a spammer's) freezes the
-- lists it owned.
CREATE FUNCTION users_freeze_lists() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE groups SET frozen_at = now() WHERE owner_user_id = NEW.id AND frozen_at IS NULL;
    RETURN NULL;
END;
$$;

CREATE TRIGGER users_freeze_lists AFTER UPDATE OF deleted_at ON users
    FOR EACH ROW WHEN (OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL)
    EXECUTE FUNCTION users_freeze_lists();

-- A frozen list's membership doesn't change. The application refuses
-- first, and this makes sure. Someone whose own account is deleted still
-- leaves it, as they leave everything.
CREATE FUNCTION group_members_refuse_frozen() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF EXISTS (SELECT 1 FROM groups WHERE id = NEW.group_id AND frozen_at IS NOT NULL) THEN
            RAISE EXCEPTION 'list % is frozen', NEW.group_id USING ERRCODE = 'check_violation';
        END IF;
        RETURN NEW;
    END IF;
    IF EXISTS (SELECT 1 FROM groups WHERE id = OLD.group_id AND frozen_at IS NOT NULL)
       AND EXISTS (SELECT 1 FROM users WHERE id = OLD.user_id AND deleted_at IS NULL) THEN
        RAISE EXCEPTION 'list % is frozen', OLD.group_id USING ERRCODE = 'check_violation';
    END IF;
    RETURN OLD;
END;
$$;

CREATE TRIGGER group_members_refuse_frozen BEFORE INSERT OR DELETE ON group_members
    FOR EACH ROW EXECUTE FUNCTION group_members_refuse_frozen();

-- Invites add people to site groups only.
UPDATE invites SET grant_group_ids = array(
    SELECT g FROM unnest(grant_group_ids) g
    WHERE EXISTS (SELECT 1 FROM groups WHERE id = g AND owner_user_id IS NULL))
WHERE grant_group_ids <> '{}';

-- The groups whose grants let a member through: those they are in, and
-- the lists they own.
CREATE FUNCTION member_group_ids(member bigint) RETURNS bigint[] LANGUAGE sql STABLE AS $$
    SELECT array(SELECT group_id FROM group_members WHERE user_id = member
                 UNION
                 SELECT id FROM groups WHERE owner_user_id = member
                 ORDER BY 1)
$$;

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
             WHERE b.user_id = u.id AND bu.role = 'user' ORDER BY 1) AS blocked_user_ids
FROM users u
WHERE u.deleted_at IS NULL;

-- How a grant's groups are named to a viewer (DESIGN.md §3.3). Site
-- groups by name, for everyone who can see the grant.
CREATE FUNCTION site_group_names(ids bigint[]) RETURNS text[] LANGUAGE sql STABLE AS $$
    SELECT array(SELECT g.name FROM groups g
                 WHERE g.id = ANY (ids) AND g.owner_user_id IS NULL
                 ORDER BY lower(g.name), g.id)
$$;

-- Lists: to their owner and the people on them as "Ruth’s Friends (12)",
-- with how many are on it; to anyone else (`viewer` NULL for visitors)
-- as "people Ruth has chosen", once per owner, so that the list's name
-- and size stay hidden. Lists the viewer knows come first.
CREATE FUNCTION list_labels(ids bigint[], viewer bigint) RETURNS text[] LANGUAGE sql STABLE AS $$
    SELECT array(
        SELECT label FROM (
            SELECT DISTINCT
                   k.known,
                   CASE WHEN k.known
                        THEN o.display_name || '’s ' || g.name || ' ('
                             || (SELECT count(*) FROM group_members m JOIN users mu ON mu.id = m.user_id
                                 WHERE m.group_id = g.id AND mu.deleted_at IS NULL) || ')'
                        ELSE 'people ' || o.display_name || ' has chosen' END AS label
            FROM groups g
            JOIN users o ON o.id = g.owner_user_id
            CROSS JOIN LATERAL (
                SELECT coalesce(g.owner_user_id = viewer
                                OR EXISTS (SELECT 1 FROM group_members m
                                           WHERE m.group_id = g.id AND m.user_id = viewer),
                                false) AS known
            ) k
            WHERE g.id = ANY (ids)
        ) x
        ORDER BY known DESC, lower(label), label)
$$;

-- The view of spaces with their grants no longer names the groups: who
-- sees which name depends on who is looking.
DROP VIEW space_targets;
CREATE VIEW space_targets AS
SELECT c.id, c.slug, c.name, c.description, c.essential,
       c.visibility, c.created_by, c.created_at, c.archived_at,
       r.level AS read_level, r.group_ids AS read_group_ids, r.user_ids AS read_user_ids,
       p.level AS reply_level, p.group_ids AS reply_group_ids, p.user_ids AS reply_user_ids,
       st.level AS start_level, st.group_ids AS start_group_ids, st.user_ids AS start_user_ids,
       a.level AS administer_level, a.group_ids AS administer_group_ids, a.user_ids AS administer_user_ids
FROM spaces c
JOIN space_grants r ON r.space_id = c.id AND r.verb = 'read'
JOIN space_grants p ON p.space_id = c.id AND p.verb = 'reply'
JOIN space_grants st ON st.space_id = c.id AND st.verb = 'start'
JOIN space_grants a ON a.space_id = c.id AND a.verb = 'administer';
