-- The core content model (DESIGN.md §4): users, sections, categories,
-- threads and posts. Later milestones add the remaining user columns,
-- groups, DMs and so on.

CREATE EXTENSION IF NOT EXISTS citext;

CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    username citext NOT NULL UNIQUE,
    display_name text NOT NULL,
    email citext NOT NULL UNIQUE,
    email_verified_at timestamptz,
    password_hash text,
    role text NOT NULL DEFAULT 'user' CHECK (role IN ('user', 'moderator', 'admin')),
    title text,
    pronouns text,
    status_line text CHECK (char_length(status_line) <= 80),
    location text,
    invited_by_user_id bigint REFERENCES users (id),
    created_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz
);

-- Display-only headings on the index page.
CREATE TABLE sections (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name text NOT NULL,
    position integer NOT NULL DEFAULT 0
);

CREATE TABLE categories (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    section_id bigint REFERENCES sections (id) ON DELETE SET NULL,
    slug text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9]+(-[a-z0-9]+)*$'),
    name text NOT NULL,
    description text NOT NULL DEFAULT '',
    position integer NOT NULL DEFAULT 0,
    read_level text NOT NULL DEFAULT 'public' CHECK (read_level IN ('public', 'members', 'groups')),
    write_level text NOT NULL DEFAULT 'members' CHECK (write_level IN ('members', 'groups', 'staff')),
    version bigint NOT NULL DEFAULT 1
);

CREATE INDEX categories_section_position ON categories (section_id, position, id);

CREATE TABLE threads (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- NULL only for DMs, which are always private.
    category_id bigint REFERENCES categories (id),
    -- A copy of the category's read level (or 'private' for DMs), kept up
    -- to date by the triggers below so that readers can filter on it.
    visibility text NOT NULL CHECK (visibility IN ('public', 'members', 'groups', 'private')),
    title text NOT NULL,
    slug text NOT NULL,
    author_id bigint NOT NULL REFERENCES users (id),
    forked_from_post_id bigint, -- references posts; constraint added below
    last_seq integer NOT NULL DEFAULT 0,
    last_post_at timestamptz NOT NULL DEFAULT now(),
    pinned boolean NOT NULL DEFAULT false,
    locked boolean NOT NULL DEFAULT false,
    deleted_at timestamptz,
    version bigint NOT NULL DEFAULT 1,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((category_id IS NULL) = (visibility = 'private'))
);

-- Category pages: pinned first, then by last activity.
CREATE INDEX threads_category_listing ON threads (category_id, pinned DESC, last_post_at DESC, id DESC)
    WHERE deleted_at IS NULL;
-- /latest and the index page's "Latest" list.
CREATE INDEX threads_latest ON threads (last_post_at DESC, id DESC) WHERE deleted_at IS NULL;
CREATE INDEX threads_forked_from ON threads (forked_from_post_id) WHERE forked_from_post_id IS NOT NULL;

CREATE TABLE posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    thread_id bigint NOT NULL REFERENCES threads (id),
    seq integer NOT NULL CHECK (seq >= 1),
    author_id bigint NOT NULL REFERENCES users (id),
    reply_to_post_id bigint REFERENCES posts (id),
    body_md text NOT NULL,
    body_html text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    edited_at timestamptz,
    deleted_at timestamptz,
    hidden_at timestamptz,
    UNIQUE (thread_id, seq)
);

CREATE INDEX posts_author ON posts (author_id);

ALTER TABLE threads
    ADD CONSTRAINT threads_forked_from_post_fk FOREIGN KEY (forked_from_post_id) REFERENCES posts (id);

-- A reply must point at a post in the same thread.
CREATE FUNCTION posts_check_reply_to() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.reply_to_post_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM posts WHERE id = NEW.reply_to_post_id AND thread_id = NEW.thread_id
    ) THEN
        RAISE EXCEPTION 'reply_to_post_id % is not in thread %', NEW.reply_to_post_id, NEW.thread_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER posts_check_reply_to BEFORE INSERT OR UPDATE OF reply_to_post_id, thread_id ON posts
    FOR EACH ROW EXECUTE FUNCTION posts_check_reply_to();

-- threads.visibility always mirrors the category's read level.
CREATE FUNCTION threads_set_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.category_id IS NULL THEN
        NEW.visibility := 'private';
    ELSE
        SELECT read_level INTO NEW.visibility FROM categories WHERE id = NEW.category_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER threads_set_visibility BEFORE INSERT OR UPDATE OF category_id, visibility ON threads
    FOR EACH ROW EXECUTE FUNCTION threads_set_visibility();

CREATE FUNCTION categories_propagate_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE threads SET visibility = NEW.read_level WHERE category_id = NEW.id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER categories_propagate_visibility AFTER UPDATE OF read_level ON categories
    FOR EACH ROW WHEN (OLD.read_level IS DISTINCT FROM NEW.read_level)
    EXECUTE FUNCTION categories_propagate_visibility();

-- The one visibility condition every content query uses (DESIGN.md §6).
-- `vis` lists the visibility levels the viewer can read everywhere, and
-- `group_category_ids` the group-restricted categories they can read.
-- It is a plain SQL expression so that the planner inlines it.
CREATE FUNCTION thread_readable(visibility text, category_id bigint, vis text[], group_category_ids bigint[])
RETURNS boolean LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT visibility = ANY (vis) OR (visibility = 'groups' AND category_id = ANY (group_category_ids))
$$;

CREATE FUNCTION category_readable(read_level text, category_id bigint, vis text[], group_category_ids bigint[])
RETURNS boolean LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT read_level = ANY (vis) OR (read_level = 'groups' AND category_id = ANY (group_category_ids))
$$;
