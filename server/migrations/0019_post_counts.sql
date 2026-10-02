-- How many posts each member has in each category, kept up to date by the
-- triggers below. Thread pages and profiles show how many of a member's
-- posts the viewer can see. Summing these counts over the categories the
-- viewer can read gives that number at a cost that doesn't grow with how
-- much the member has posted, where counting their posts on every page did.
--
-- A post counts when it is neither deleted nor hidden and its thread is a
-- live thread in a category. Conversations never count: their threads have
-- no category.
CREATE TABLE user_category_post_counts (
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    category_id bigint NOT NULL REFERENCES categories (id) ON DELETE CASCADE,
    posts bigint NOT NULL,
    PRIMARY KEY (user_id, category_id)
);

INSERT INTO user_category_post_counts (user_id, category_id, posts)
SELECT p.author_id, t.category_id, count(*)
FROM posts p JOIN threads t ON t.id = p.thread_id
WHERE t.category_id IS NOT NULL AND t.deleted_at IS NULL
  AND p.deleted_at IS NULL AND p.hidden_at IS NULL
GROUP BY p.author_id, t.category_id;

CREATE FUNCTION adjust_post_count(member bigint, category bigint, delta bigint)
RETURNS void LANGUAGE sql AS $$
    INSERT INTO user_category_post_counts (user_id, category_id, posts)
    VALUES (member, category, delta)
    ON CONFLICT (user_id, category_id)
    DO UPDATE SET posts = user_category_post_counts.posts + EXCLUDED.posts
$$;

-- The category a post in `thread` counts in, or NULL if it doesn't count.
CREATE FUNCTION post_counted_in(thread bigint, deleted timestamptz, hidden timestamptz)
RETURNS bigint LANGUAGE sql STABLE AS $$
    SELECT t.category_id FROM threads t
    WHERE t.id = thread AND t.deleted_at IS NULL AND deleted IS NULL AND hidden IS NULL
$$;

CREATE FUNCTION count_post_changes() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    category bigint;
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        category := post_counted_in(OLD.thread_id, OLD.deleted_at, OLD.hidden_at);
        IF category IS NOT NULL THEN
            PERFORM adjust_post_count(OLD.author_id, category, -1);
        END IF;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        category := post_counted_in(NEW.thread_id, NEW.deleted_at, NEW.hidden_at);
        IF category IS NOT NULL THEN
            PERFORM adjust_post_count(NEW.author_id, category, 1);
        END IF;
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER posts_count_changes
AFTER INSERT OR DELETE OR UPDATE OF thread_id, author_id, deleted_at, hidden_at ON posts
FOR EACH ROW EXECUTE FUNCTION count_post_changes();

-- A thread moving category, or being deleted or restored, moves all of its
-- counted posts at once. (Threads with posts can't be deleted outright:
-- posts reference them without cascading.)
CREATE FUNCTION count_thread_changes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.category_id IS NOT NULL AND OLD.deleted_at IS NULL THEN
        INSERT INTO user_category_post_counts (user_id, category_id, posts)
        SELECT p.author_id, OLD.category_id, -count(*)
        FROM posts p
        WHERE p.thread_id = OLD.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
        GROUP BY p.author_id
        ON CONFLICT (user_id, category_id)
        DO UPDATE SET posts = user_category_post_counts.posts + EXCLUDED.posts;
    END IF;
    IF NEW.category_id IS NOT NULL AND NEW.deleted_at IS NULL THEN
        INSERT INTO user_category_post_counts (user_id, category_id, posts)
        SELECT p.author_id, NEW.category_id, count(*)
        FROM posts p
        WHERE p.thread_id = NEW.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
        GROUP BY p.author_id
        ON CONFLICT (user_id, category_id)
        DO UPDATE SET posts = user_category_post_counts.posts + EXCLUDED.posts;
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER threads_count_changes
AFTER UPDATE OF category_id, deleted_at ON threads
FOR EACH ROW
WHEN (OLD.category_id IS DISTINCT FROM NEW.category_id OR OLD.deleted_at IS DISTINCT FROM NEW.deleted_at)
EXECUTE FUNCTION count_thread_changes();
