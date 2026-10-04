-- Feeds (DESIGN.md §3.2, §4; milestone 13). A feed is a space of kind
-- `feed` with an owner. Every member has one from the moment their account
-- exists: its address is their username (in lower case, so a feed's slug
-- may hold the underscores usernames can), its name is their display name,
-- members read and reply, and only the owner starts threads and runs it.
-- The owner always runs it: its administer grant names them, whatever else
-- it says, or lets every member through.
--
-- A feed's address follows its owner's username, and its name follows
-- their display name for as long as the two are the same. When the
-- owner's account is deleted, the feed is archived (readable as before,
-- closed to new posts) and no longer says whose it was.
--
-- Topic spaces and usernames share a namespace (§3.1). A topic space whose
-- address is already some member's username moves to the first free
-- address ending "-space", and the people who run it, and the admins, are
-- told.

ALTER TABLE spaces
    ADD COLUMN kind text NOT NULL DEFAULT 'topic' CHECK (kind IN ('topic', 'feed')),
    ADD COLUMN owner_user_id bigint REFERENCES users (id),
    ADD CONSTRAINT spaces_feed_owner CHECK ((kind = 'feed') = (owner_user_id IS NOT NULL));

CREATE UNIQUE INDEX spaces_one_feed ON spaces (owner_user_id) WHERE kind = 'feed';

ALTER TABLE spaces DROP CONSTRAINT spaces_slug_check;
ALTER TABLE spaces ADD CONSTRAINT spaces_slug_check
    CHECK (slug ~ '^[a-z0-9]+(-[a-z0-9]+)*$' OR (kind = 'feed' AND slug ~ '^[a-z0-9_]+$'));

-- Topic spaces at a member's name move aside.
DO $$
DECLARE
    s record;
    target text;
    n integer;
BEGIN
    FOR s IN SELECT c.id, c.slug, c.name, c.created_by
             FROM spaces c
             WHERE EXISTS (SELECT 1 FROM users u
                           WHERE u.deleted_at IS NULL AND lower(u.username::text) = c.slug)
             ORDER BY c.id
    LOOP
        target := s.slug || '-space';
        n := 1;
        WHILE EXISTS (SELECT 1 FROM spaces WHERE slug = target)
              OR EXISTS (SELECT 1 FROM users WHERE lower(username::text) = target) LOOP
            n := n + 1;
            target := s.slug || '-space-' || n;
        END LOOP;
        UPDATE spaces SET slug = target, version = version + 1 WHERE id = s.id;
        INSERT INTO notifications (user_id, kind, message)
        SELECT u.id, 'moderation',
               'Every member now has a space of their own, a feed, at their username, so the space “'
               || s.name || '” has moved from /c/' || s.slug || ' to /c/' || target
               || ', since ' || s.slug || ' is a member’s username. Links to the old address now lead to their own feed.'
        FROM users u
        WHERE u.deleted_at IS NULL
          AND (u.role = 'admin'
               OR u.id = s.created_by
               OR EXISTS (SELECT 1 FROM space_grants g
                          WHERE g.space_id = s.id AND g.verb = 'administer' AND g.level = 'listed'
                            AND g.user_ids @> ARRAY[u.id]))
        ORDER BY u.id;
    END LOOP;
END;
$$;

-- Makes a member's feed, with its grants, returning its id.
CREATE FUNCTION make_feed(member bigint) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    feed bigint;
BEGIN
    INSERT INTO spaces (slug, name, description, kind, owner_user_id, created_by, visibility, created_at)
    SELECT lower(u.username::text), u.display_name, '', 'feed', u.id, u.id, 'restricted', u.created_at
    FROM users u WHERE u.id = member
    RETURNING id INTO feed;
    INSERT INTO space_grants (space_id, verb, level, user_ids) VALUES
        (feed, 'read', 'members', '{}'),
        (feed, 'reply', 'members', '{}'),
        (feed, 'start', 'listed', ARRAY[member]),
        (feed, 'administer', 'listed', ARRAY[member]);
    RETURN feed;
END;
$$;

-- Puts a member's own feed in their layout's "Feeds" section (the first
-- so named), made at the end if they have none, unless the feed is in one
-- of their sections already.
CREATE FUNCTION layout_add_own_feed(target bigint) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    owner bigint;
    feed bigint;
    feeds bigint;
BEGIN
    SELECT user_id INTO owner FROM layouts WHERE id = target FOR UPDATE;
    SELECT id INTO feed FROM spaces WHERE kind = 'feed' AND owner_user_id = owner;
    IF feed IS NULL OR EXISTS (SELECT 1 FROM layout_section_spaces x
                               JOIN layout_sections s ON s.id = x.section_id
                               WHERE s.layout_id = target AND x.space_id = feed) THEN
        RETURN;
    END IF;
    SELECT id INTO feeds FROM layout_sections
    WHERE layout_id = target AND name = 'Feeds'
    ORDER BY position, id LIMIT 1;
    IF feeds IS NULL THEN
        INSERT INTO layout_sections (layout_id, name, position)
        SELECT target, 'Feeds', coalesce(max(position), -1) + 1
        FROM layout_sections WHERE layout_id = target
        RETURNING id INTO feeds;
    END IF;
    INSERT INTO layout_section_spaces (section_id, space_id, position)
    SELECT feeds, feed, coalesce(max(position), -1) + 1
    FROM layout_section_spaces WHERE section_id = feeds;
END;
$$;

-- Every member whose account exists gets one now, in their Feeds section.
DO $$
DECLARE
    member bigint;
    l bigint;
BEGIN
    FOR member IN SELECT id FROM users WHERE deleted_at IS NULL ORDER BY id LOOP
        PERFORM make_feed(member);
        SELECT id INTO l FROM layouts WHERE user_id = member;
        IF l IS NOT NULL THEN
            PERFORM layout_add_own_feed(l);
        END IF;
    END LOOP;
END;
$$;

-- And every new account, wherever accounts are made. (Its layout is made
-- after the account, and puts the feed in its Feeds section then.)
CREATE FUNCTION users_make_feed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM make_feed(NEW.id);
    RETURN NULL;
END;
$$;

CREATE TRIGGER users_make_feed AFTER INSERT ON users
    FOR EACH ROW EXECUTE FUNCTION users_make_feed();

-- The feed follows its owner: its address is their username, its name
-- their display name while it has not been given another, and deleting
-- the account archives it and takes their name off it.
CREATE FUNCTION users_update_feed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE spaces
    SET slug = lower(NEW.username::text),
        name = CASE WHEN NEW.deleted_at IS NOT NULL OR name = OLD.display_name
                    THEN NEW.display_name ELSE name END,
        description = CASE WHEN NEW.deleted_at IS NOT NULL THEN '' ELSE description END,
        archived_at = CASE WHEN NEW.deleted_at IS NOT NULL THEN coalesce(archived_at, now())
                           ELSE archived_at END,
        version = version + 1
    WHERE kind = 'feed' AND owner_user_id = NEW.id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER users_update_feed AFTER UPDATE OF username, display_name, deleted_at ON users
    FOR EACH ROW WHEN (OLD.username IS DISTINCT FROM NEW.username
                       OR OLD.display_name IS DISTINCT FROM NEW.display_name
                       OR OLD.deleted_at IS DISTINCT FROM NEW.deleted_at)
    EXECUTE FUNCTION users_update_feed();

-- A space stays the kind it was made, and a feed stays its owner's.
CREATE FUNCTION spaces_keep_kind() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.kind IS DISTINCT FROM OLD.kind OR NEW.owner_user_id IS DISTINCT FROM OLD.owner_user_id THEN
        RAISE EXCEPTION 'space % can''t change its kind or owner', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER spaces_keep_kind BEFORE UPDATE OF kind, owner_user_id ON spaces
    FOR EACH ROW EXECUTE FUNCTION spaces_keep_kind();

-- The owner always runs their feed. The application names them first, and
-- this makes sure.
CREATE FUNCTION space_grants_feed_owner() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    owner bigint;
BEGIN
    IF NEW.verb <> 'administer' THEN
        RETURN NEW;
    END IF;
    SELECT owner_user_id INTO owner FROM spaces WHERE id = NEW.space_id AND kind = 'feed';
    IF owner IS NOT NULL
       AND NOT (NEW.level = 'members' OR (NEW.level = 'listed' AND NEW.user_ids @> ARRAY[owner])) THEN
        RAISE EXCEPTION 'the owner of feed % always runs it', NEW.space_id USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER space_grants_feed_owner BEFORE INSERT OR UPDATE ON space_grants
    FOR EACH ROW EXECUTE FUNCTION space_grants_feed_owner();

-- Whether a space appears in lists of spaces for `viewer` (NULL for
-- nobody in particular): a topic space always; a feed once it has a
-- thread that isn't deleted, and always for its owner.
CREATE FUNCTION space_in_view(kind text, owner bigint, space bigint, viewer bigint)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT kind = 'topic'
        OR owner = viewer
        OR EXISTS (SELECT 1 FROM threads t WHERE t.space_id = space AND t.deleted_at IS NULL)
$$;

-- The view of spaces with their grants says what kind each is, and
-- whose a feed is.
DROP VIEW space_targets;
CREATE VIEW space_targets AS
SELECT c.id, c.slug, c.name, c.description, c.essential,
       c.visibility, c.created_by, c.created_at, c.archived_at,
       r.level AS read_level, r.group_ids AS read_group_ids, r.user_ids AS read_user_ids,
       p.level AS reply_level, p.group_ids AS reply_group_ids, p.user_ids AS reply_user_ids,
       st.level AS start_level, st.group_ids AS start_group_ids, st.user_ids AS start_user_ids,
       a.level AS administer_level, a.group_ids AS administer_group_ids, a.user_ids AS administer_user_ids,
       c.kind, c.owner_user_id
FROM spaces c
JOIN space_grants r ON r.space_id = c.id AND r.verb = 'read'
JOIN space_grants p ON p.space_id = c.id AND p.verb = 'reply'
JOIN space_grants st ON st.space_id = c.id AND st.verb = 'start'
JOIN space_grants a ON a.space_id = c.id AND a.verb = 'administer';
