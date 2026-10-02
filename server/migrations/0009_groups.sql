-- Groups (DESIGN.md §3.1, §4). Admins make groups and add members to
-- them, and invites can add the people who use them. A category whose read
-- level is 'groups' can be read by the members of the groups given 'read'
-- access to it here (and by staff); one whose write level is 'groups' can
-- be posted in by the members of the groups given 'write' access.

CREATE TABLE groups (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    slug text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9]+(-[a-z0-9]+)*$' AND char_length(slug) <= 40),
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 60),
    description text NOT NULL DEFAULT '' CHECK (char_length(description) <= 300),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Group names label categories, so two groups can't share one.
CREATE UNIQUE INDEX groups_name ON groups (lower(name));

CREATE TABLE group_members (
    group_id bigint NOT NULL REFERENCES groups (id) ON DELETE CASCADE,
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    added_by bigint REFERENCES users (id) ON DELETE SET NULL,
    added_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (group_id, user_id)
);

-- Looking up a member's groups when their session is checked.
CREATE INDEX group_members_user ON group_members (user_id);

CREATE TABLE category_groups (
    category_id bigint NOT NULL REFERENCES categories (id) ON DELETE CASCADE,
    group_id bigint NOT NULL REFERENCES groups (id) ON DELETE CASCADE,
    access text NOT NULL CHECK (access IN ('read', 'write')),
    PRIMARY KEY (category_id, group_id, access)
);

CREATE INDEX category_groups_group ON category_groups (group_id, access);

-- Invites name the groups they add people to in an array, which can't
-- have a foreign key; a deleted group is taken out of them instead.
CREATE FUNCTION groups_forget_in_invites() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE invites SET grant_group_ids = array_remove(grant_group_ids, OLD.id)
    WHERE OLD.id = ANY (grant_group_ids);
    RETURN NULL;
END;
$$;

CREATE TRIGGER groups_forget_in_invites AFTER DELETE ON groups
    FOR EACH ROW EXECUTE FUNCTION groups_forget_in_invites();
