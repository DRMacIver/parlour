-- Fewer notifications (DESIGN.md §2, §4, §9; milestone 11). Watches
-- become thread subscriptions: a member hears about new posts in the
-- threads they are subscribed to (the ones they start, and any they
-- subscribe to from the thread's page), about mentions of them, about
-- messages, and about notices concerning them. Nothing subscribes anyone
-- to a space, and nobody is told about replies to their posts as such.

CREATE TABLE thread_subscriptions (
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    thread_id bigint NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, thread_id)
);

CREATE INDEX thread_subscriptions_thread ON thread_subscriptions (thread_id);

-- Watching a thread in a space was being told of every new post in it.
INSERT INTO thread_subscriptions (user_id, thread_id)
SELECT w.user_id, w.thread_id
FROM watches w JOIN threads t ON t.id = w.thread_id
WHERE w.level = 'watching' AND t.space_id IS NOT NULL;

-- Everyone in a conversation was told of every message in it unless they
-- had muted it: now they are subscribed to it, and unsubscribing is how
-- they stop hearing about it.
INSERT INTO thread_subscriptions (user_id, thread_id)
SELECT p.user_id, p.thread_id
FROM private_thread_participants p
WHERE p.left_at IS NULL
  AND NOT EXISTS (SELECT 1 FROM watches w
                  WHERE w.user_id = p.user_id AND w.thread_id = p.thread_id AND w.level = 'muted')
ON CONFLICT DO NOTHING;

-- Spaces can no longer be watched or muted for notifications. Everyone
-- who had chosen either for some space is told once.
INSERT INTO notifications (user_id, kind, message)
SELECT u.id, 'moderation',
       'Notifications have changed: spaces can no longer be watched or muted for notifications, so '
       || 'your choices for spaces have been removed. You now hear about mentions of you, new posts '
       || 'in threads you are subscribed to, and messages. You are subscribed to the threads you start, '
       || 'and you can subscribe to any other thread, or unsubscribe, from its page.'
FROM users u
WHERE u.deleted_at IS NULL
  AND EXISTS (SELECT 1 FROM watches w WHERE w.user_id = u.id AND w.space_id IS NOT NULL);

DROP TABLE watches;

-- The kinds of notification there are now. What was stored under the old
-- kinds was always a new post in a thread, so it is kept as one: "Ruth
-- posted in “Brass”" is true of a reply to the member, a post in a thread
-- they watched, and a new thread in a space they watched.
ALTER TABLE notifications DROP CONSTRAINT notifications_kind_check;
UPDATE notifications SET kind = 'subscription' WHERE kind IN ('reply', 'watched_thread', 'watched_space');
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check
    CHECK (kind IN ('mention', 'subscription', 'message', 'moderation'));

-- Email preferences: "threads and spaces you watch" becomes "threads you
-- subscribe to", keeping the member's choice; replies to their posts are
-- no longer a kind of their own.
UPDATE users
SET email_prefs = (email_prefs - 'replies' - 'watching')
    || CASE WHEN email_prefs ? 'watching'
            THEN jsonb_build_object('subscriptions', email_prefs -> 'watching')
            ELSE '{}'::jsonb END
WHERE email_prefs ?| ARRAY['replies', 'watching'];

-- `/t/{id}/subscribe` and `/t/{id}/unsubscribe` are routes now, so a
-- thread can't have either as its slug.
UPDATE threads SET slug = slug || '-thread' WHERE slug IN ('subscribe', 'unsubscribe');
