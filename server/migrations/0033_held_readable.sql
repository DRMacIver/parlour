-- Break glass (milestone 15): a post held for approval is shown to a
-- moderator only where they could read it once it is up (DESIGN.md §1
-- principle 3, §8). Its author sees their own wherever it would go.
--
-- Whether a viewer could read where a held post would go: for a reply,
-- the thread (or conversation) it is in; for a new thread, its space,
-- and the read restriction its author chose, if any, as `thread_readable`
-- reads a thread's. Takes the viewer's parameters as `thread_readable`
-- does.
CREATE FUNCTION held_readable(h held_posts, vis text[], restricted_space_ids bigint[],
                              reader bigint, reader_groups bigint[])
RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT CASE
        WHEN h.thread_id IS NOT NULL THEN EXISTS (
            SELECT 1 FROM threads t
            WHERE t.id = h.thread_id
              AND (thread_readable(t, vis, restricted_space_ids, reader, reader_groups)
                   OR dm_readable(t.id, t.visibility, reader)))
        ELSE EXISTS (
            SELECT 1 FROM spaces c
            WHERE c.id = h.space_id
              AND space_readable(c.visibility, c.id, vis, restricted_space_ids))
          AND (h.read_user_ids IS NULL
               OR coalesce(reader = ANY (h.read_user_ids), false)
               OR h.read_group_ids && coalesce(reader_groups, '{}'))
    END
$$;
