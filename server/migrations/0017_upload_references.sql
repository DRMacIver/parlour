-- More places an image may be seen (DESIGN.md §9), alongside the posts
-- that show it (post_uploads):
--
-- - the earlier versions of posts, which anyone who may read the post may
--   read on its edit history, until a moderator redacts the version;
-- - posts held for approval, which staff review before anyone else sees
--   them.

CREATE TABLE revision_uploads (
    revision_id bigint NOT NULL REFERENCES post_revisions (id) ON DELETE CASCADE,
    sha256 text NOT NULL,
    PRIMARY KEY (revision_id, sha256)
);

CREATE INDEX revision_uploads_sha256 ON revision_uploads (sha256);

CREATE FUNCTION post_revisions_record_uploads() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM revision_uploads WHERE revision_id = NEW.id;
    INSERT INTO revision_uploads (revision_id, sha256)
        SELECT DISTINCT NEW.id, m[1]
        FROM regexp_matches(NEW.body_html, '/uploads/([0-9a-f]{64})\.(?:png|jpg|gif)', 'g') AS m;
    RETURN NULL;
END;
$$;

CREATE TRIGGER post_revisions_record_uploads AFTER INSERT OR UPDATE OF body_html ON post_revisions
    FOR EACH ROW EXECUTE FUNCTION post_revisions_record_uploads();

INSERT INTO revision_uploads (revision_id, sha256)
    SELECT DISTINCT r.id, m[1]
    FROM post_revisions r,
         regexp_matches(r.body_html, '/uploads/([0-9a-f]{64})\.(?:png|jpg|gif)', 'g') AS m;

-- A held post is kept as Markdown, and rendered when it is looked at.
CREATE TABLE held_post_uploads (
    held_post_id bigint NOT NULL REFERENCES held_posts (id) ON DELETE CASCADE,
    sha256 text NOT NULL,
    PRIMARY KEY (held_post_id, sha256)
);

CREATE INDEX held_post_uploads_sha256 ON held_post_uploads (sha256);

CREATE FUNCTION held_posts_record_uploads() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM held_post_uploads WHERE held_post_id = NEW.id;
    INSERT INTO held_post_uploads (held_post_id, sha256)
        SELECT DISTINCT NEW.id, m[1]
        FROM regexp_matches(NEW.body_md, '/uploads/([0-9a-f]{64})\.(?:png|jpg|gif)', 'g') AS m;
    RETURN NULL;
END;
$$;

CREATE TRIGGER held_posts_record_uploads AFTER INSERT OR UPDATE OF body_md ON held_posts
    FOR EACH ROW EXECUTE FUNCTION held_posts_record_uploads();

INSERT INTO held_post_uploads (held_post_id, sha256)
    SELECT DISTINCT h.id, m[1]
    FROM held_posts h,
         regexp_matches(h.body_md, '/uploads/([0-9a-f]{64})\.(?:png|jpg|gif)', 'g') AS m;
