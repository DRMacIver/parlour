-- Break glass, the notices (milestone 15, DESIGN.md §8, §9): everyone who
-- can read what a staff member broke glass on is told at once, by a
-- notification of its own kind that names the moderator and the day but
-- not why, nor who reported it.

ALTER TABLE notifications DROP CONSTRAINT notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check
    CHECK (kind IN ('mention', 'subscription', 'message', 'moderation', 'staff_access'));

-- The grant a notice is about. A row of the log is never removed, so the
-- notice keeps it.
ALTER TABLE notifications ADD COLUMN staff_access_id bigint REFERENCES staff_access (id);
ALTER TABLE notifications ADD CONSTRAINT notifications_staff_access
    CHECK ((kind = 'staff_access') = (staff_access_id IS NOT NULL));
ALTER TABLE notifications ADD CONSTRAINT notifications_staff_access_no_post
    CHECK (kind <> 'staff_access' OR post_id IS NULL);
-- Only notes from the moderators and these are about no post.
ALTER TABLE notifications DROP CONSTRAINT notifications_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_about_a_post
    CHECK (post_id IS NOT NULL OR kind IN ('moderation', 'staff_access'));
CREATE UNIQUE INDEX notifications_one_per_access ON notifications (user_id, staff_access_id)
    WHERE staff_access_id IS NOT NULL;

-- Whether a viewer reads what a grant opened: its thread (or
-- conversation), or its space. A notice about a grant, like one about a
-- post, is shown only to those who can read what it is about, now: it
-- names it. Takes the viewer's parameters as `thread_readable` does.
CREATE FUNCTION staff_access_readable(access_id bigint, vis text[], restricted_space_ids bigint[],
                                      reader bigint, reader_groups bigint[])
RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT EXISTS (
        SELECT 1 FROM staff_access a
        LEFT JOIN threads t ON t.id = a.thread_id
        LEFT JOIN spaces c ON c.id = a.space_id
        WHERE a.id = access_id
          AND ((t.id IS NOT NULL AND t.deleted_at IS NULL
                AND (thread_readable(t, vis, restricted_space_ids, reader, reader_groups)
                     OR dm_readable(t.id, t.visibility, reader)))
               OR (c.id IS NOT NULL
                   AND space_readable(c.visibility, c.id, vis, restricted_space_ids))))
$$;

-- The badge counts a notice about a grant while its reader can read what
-- it is about, as it counts one about a post.
DROP FUNCTION unread_notification_count(bigint, text[], bigint[], bigint, bigint[]);
CREATE FUNCTION unread_notification_count(member bigint, vis text[], restricted_space_ids bigint[],
                                          dm_viewer bigint, reader_groups bigint[])
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT count(*)
    FROM notifications n
    LEFT JOIN posts p ON p.id = n.post_id
    LEFT JOIN threads t ON t.id = p.thread_id
    WHERE n.user_id = member AND n.read_at IS NULL
      AND CASE
          WHEN n.staff_access_id IS NOT NULL
              THEN staff_access_readable(n.staff_access_id, vis, restricted_space_ids,
                                         dm_viewer, reader_groups)
          WHEN n.post_id IS NULL THEN true
          ELSE p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
               AND (thread_readable(t, vis, restricted_space_ids, dm_viewer, reader_groups)
                    OR dm_readable(t.id, t.visibility, dm_viewer))
          END
      AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                      WHERE b.user_id = member AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
$$;
