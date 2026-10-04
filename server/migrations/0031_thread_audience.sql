-- Per-thread permissions, the forms (milestone 14): a thread's audience
-- page and restricting a new thread from the start (DESIGN.md §3.4).
--
-- A new member's thread may be held for approval (DESIGN.md §7) with the
-- read restriction its author chose, which it takes when it is approved:
-- the held post keeps the groups and people named, NULL together when the
-- thread is to have its space's audience. Only new threads carry one.

ALTER TABLE held_posts
    ADD COLUMN read_group_ids bigint[],
    ADD COLUMN read_user_ids bigint[],
    ADD CONSTRAINT held_posts_read_parts CHECK ((read_group_ids IS NULL) = (read_user_ids IS NULL)),
    ADD CONSTRAINT held_posts_read_new_threads CHECK (read_group_ids IS NULL OR space_id IS NOT NULL);

-- `/t/{id}/audience` is the thread's audience page, so no thread's
-- address may end in `audience`, as none ends in `unread` or `reply`
-- (`store::slugify` keeps new ones from it).
UPDATE threads SET slug = 'audience-thread' WHERE slug = 'audience';

-- The people a restriction names one by one, by display name, for the
-- lock and the audience beside a restricted thread's title and in thread
-- lists (DESIGN.md §3.4, §5). Deleted accounts are left out.
CREATE FUNCTION people_names(ids bigint[]) RETURNS text[] LANGUAGE sql STABLE AS $$
    SELECT array(SELECT u.display_name FROM users u
                 WHERE u.id = ANY (ids) AND u.deleted_at IS NULL
                 ORDER BY lower(u.display_name), u.id)
$$;
