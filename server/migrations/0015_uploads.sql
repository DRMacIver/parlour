-- Uploaded images (DESIGN.md §2, §9). The file is stored once, named by
-- the SHA-256 of its re-encoded bytes; each member who uploaded it has a
-- row here. An image may be fetched by its uploader, by anyone if it is a
-- member's avatar, and otherwise by whoever may read a post it is in.
CREATE TABLE uploads (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id bigint REFERENCES users (id) ON DELETE SET NULL,
    sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    ext text NOT NULL CHECK (ext IN ('png', 'jpg', 'gif')),
    mime text NOT NULL CHECK (mime IN ('image/png', 'image/jpeg', 'image/gif')),
    bytes integer NOT NULL CHECK (bytes > 0),
    width integer NOT NULL CHECK (width > 0),
    height integer NOT NULL CHECK (height > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, sha256)
);

CREATE INDEX uploads_sha256 ON uploads (sha256);

ALTER TABLE users ADD COLUMN avatar_upload_id bigint REFERENCES uploads (id) ON DELETE SET NULL;
CREATE INDEX users_avatar ON users (avatar_upload_id) WHERE avatar_upload_id IS NOT NULL;

-- The images each post shows, kept up to date from its HTML whenever it
-- is saved, however it is saved.
CREATE TABLE post_uploads (
    post_id bigint NOT NULL REFERENCES posts (id),
    sha256 text NOT NULL,
    PRIMARY KEY (post_id, sha256)
);

CREATE INDEX post_uploads_sha256 ON post_uploads (sha256);

CREATE FUNCTION posts_record_uploads() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM post_uploads WHERE post_id = NEW.id;
    INSERT INTO post_uploads (post_id, sha256)
        SELECT DISTINCT NEW.id, m[1]
        FROM regexp_matches(NEW.body_html, '/uploads/([0-9a-f]{64})\.(?:png|jpg|gif)', 'g') AS m;
    RETURN NULL;
END;
$$;

CREATE TRIGGER posts_record_uploads AFTER INSERT OR UPDATE OF body_html ON posts
    FOR EACH ROW EXECUTE FUNCTION posts_record_uploads();
