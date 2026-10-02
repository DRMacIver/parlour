-- Definitions shared by queries that used to run separately on every page,
-- so that pages can fetch what they need in fewer round trips without
-- repeating the logic.

-- Everything about a member that decides what they may see and do, as
-- `sessions::user_ctx` and the session lookup load it: their role, their
-- groups and the categories those groups may read, and the members whose
-- posts they have asked not to see (those they blocked, unless they are
-- staff now: staff can't be blocked). Deleted accounts aren't members.
CREATE VIEW member_contexts AS
SELECT u.id, u.username, u.display_name, u.email,
       u.email_verified_at IS NOT NULL AS verified,
       u.role, u.created_at, u.silenced_until, u.suspended_until,
       array(SELECT m.group_id FROM group_members m WHERE m.user_id = u.id ORDER BY 1) AS group_ids,
       array(SELECT DISTINCT cg.category_id FROM category_groups cg
             JOIN group_members m ON m.group_id = cg.group_id
             WHERE m.user_id = u.id AND cg.access = 'read' ORDER BY 1) AS group_category_ids,
       array(SELECT b.blocked_user_id FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
             WHERE b.user_id = u.id AND bu.role = 'user' ORDER BY 1) AS blocked_user_ids
FROM users u
WHERE u.deleted_at IS NULL;

-- A category with everything needed to decide who may read and post in
-- it, and to label it.
CREATE VIEW category_targets AS
SELECT c.id, c.slug, c.name, c.description, c.position, c.section_id,
       s.name AS section_name, s.position AS section_position,
       c.read_level, c.write_level,
       array(SELECT cg.group_id FROM category_groups cg
             WHERE cg.category_id = c.id AND cg.access = 'read' ORDER BY 1) AS read_groups,
       array(SELECT cg.group_id FROM category_groups cg
             WHERE cg.category_id = c.id AND cg.access = 'write' ORDER BY 1) AS write_groups,
       array(SELECT g.name FROM category_groups cg JOIN groups g ON g.id = cg.group_id
             WHERE cg.category_id = c.id AND cg.access = 'write' AND c.write_level = 'groups'
             ORDER BY lower(g.name), g.id) AS write_group_names,
       array(SELECT g.name FROM category_groups cg JOIN groups g ON g.id = cg.group_id
             WHERE cg.category_id = c.id AND cg.access = 'read' AND c.read_level = 'groups'
             ORDER BY lower(g.name), g.id) AS group_names
FROM categories c LEFT JOIN sections s ON s.id = c.section_id;

-- The counts in the page header.

-- Unread notifications a member may still see: about posts they can read,
-- or moderation notices, and not from members they blocked.
CREATE FUNCTION unread_notification_count(member bigint, vis text[], group_category_ids bigint[], dm_viewer bigint)
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT count(*)
    FROM notifications n
    LEFT JOIN posts p ON p.id = n.post_id
    LEFT JOIN threads t ON t.id = p.thread_id
    WHERE n.user_id = member AND n.read_at IS NULL
      AND (n.post_id IS NULL
           OR (p.deleted_at IS NULL AND p.hidden_at IS NULL AND t.deleted_at IS NULL
               AND (thread_readable(t.visibility, t.category_id, vis, group_category_ids)
                    OR dm_readable(t.id, t.visibility, dm_viewer))))
      AND NOT EXISTS (SELECT 1 FROM blocks b JOIN users bu ON bu.id = b.blocked_user_id
                      WHERE b.user_id = member AND b.blocked_user_id = n.actor_id AND bu.role = 'user')
$$;

-- Conversations a member is in with messages they haven't read.
CREATE FUNCTION unread_conversation_count(member bigint)
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT count(*) FROM private_thread_participants me
    JOIN threads t ON t.id = me.thread_id
    LEFT JOIN thread_reads tr ON tr.thread_id = t.id AND tr.user_id = member
    WHERE me.user_id = member AND me.left_at IS NULL AND t.deleted_at IS NULL
      AND dm_readable(t.id, t.visibility, member)
      AND t.last_seq > coalesce(tr.last_read_seq, 0)
$$;

-- What is waiting in the moderation queue: open reports (one per post or
-- member reported, however many times) and held posts.
CREATE FUNCTION moderation_queue_size()
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT (SELECT count(DISTINCT CASE kind WHEN 'post' THEN 'p' || post_id
                                            WHEN 'user' THEN 'u' || user_id
                                            ELSE 'r' || id END)
            FROM reports WHERE status = 'open')
         + (SELECT count(*) FROM held_posts h JOIN users u ON u.id = h.author_id
            WHERE h.status = 'held' AND u.deleted_at IS NULL)
$$;

-- The moderation action behind a member's current silence or suspension,
-- for contesting it, if there is one.
CREATE FUNCTION current_restriction_action(member bigint)
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT a.id FROM users u
    JOIN mod_actions a
      ON a.action IN ('silence', 'suspend', 'suspend_branch')
     AND (a.user_id = u.id
          OR (a.action = 'suspend_branch' AND a.details->'user_ids' @> to_jsonb(u.id)))
    WHERE u.id = member
      AND CASE WHEN u.suspended_until > now()
                 THEN a.action <> 'silence'
                      AND abs(extract(epoch FROM (a.details->>'until')::timestamptz - u.suspended_until)) < 0.001
               WHEN u.silenced_until > now()
                 THEN a.action = 'silence'
                      AND abs(extract(epoch FROM (a.details->>'until')::timestamptz - u.silenced_until)) < 0.001
               ELSE false END
    ORDER BY a.created_at DESC, a.id DESC LIMIT 1
$$;
