-- Full-text search (DESIGN.md §9): generated tsvectors on posts and thread
-- titles, queried with websearch_to_tsquery. Only the current text of a
-- post is searchable; earlier versions in post_revisions are not.
ALTER TABLE posts ADD COLUMN search tsvector
    GENERATED ALWAYS AS (to_tsvector('english'::regconfig, body_md)) STORED;
ALTER TABLE threads ADD COLUMN search tsvector
    GENERATED ALWAYS AS (to_tsvector('english'::regconfig, title)) STORED;

CREATE INDEX posts_search ON posts USING gin (search);
CREATE INDEX threads_search ON threads USING gin (search);

-- Conversations are found by searching only by the people in them now
-- (DESIGN.md §3.7): unlike `dm_readable`, a report doesn't open them to
-- staff's searches.
CREATE FUNCTION dm_searchable(thread_id bigint, visibility text, viewer_id bigint)
RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT visibility = 'private' AND viewer_id IS NOT NULL AND EXISTS (
        SELECT 1 FROM private_thread_participants p
        WHERE p.thread_id = dm_searchable.thread_id AND p.user_id = viewer_id AND p.left_at IS NULL)
$$;
