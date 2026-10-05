-- Break glass, who a notice is for (DESIGN.md §8): everyone it was sent
-- to, for good. A change of who may read something tells those who could
-- read it before the change, some of whom it may shut out; they were the
-- ones whose privacy it touched, so their notice stays listed, counted
-- and emailed, naming what they can no longer read without its title.
-- The notification row is the record of who was told, and so of whose
-- log (`/settings/access`) a grant is in.

CREATE OR REPLACE FUNCTION unread_notification_count(member bigint, vis text[],
                                                     restricted_space_ids bigint[],
                                                     dm_viewer bigint, reader_groups bigint[])
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT count(*)
    FROM notifications n
    LEFT JOIN posts p ON p.id = n.post_id
    LEFT JOIN threads t ON t.id = p.thread_id
    WHERE n.user_id = member AND n.read_at IS NULL
      AND CASE
          WHEN n.post_id IS NULL THEN true
          ELSE p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
               AND (thread_readable(t, vis, restricted_space_ids, dm_viewer, reader_groups)
                    OR dm_readable(t.id, t.visibility, dm_viewer))
          END
      AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                      WHERE b.user_id = member AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
$$;

-- Nothing asks whether a member reads what a grant opened any more.
DROP FUNCTION staff_access_readable(bigint, text[], bigint[], bigint, bigint[]);
