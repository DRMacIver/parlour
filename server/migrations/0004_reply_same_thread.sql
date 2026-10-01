-- A reply must be in the same thread as the post it replies to, in both
-- directions: the old trigger only checked the replying post, so moving the
-- post replied to into another thread went unchecked.
--
-- The check now runs after the whole statement (a constraint trigger), so a
-- statement that moves a post together with its replies, as a split does, is
-- allowed. It is DEFERRABLE so that a multi-statement move can
-- `SET CONSTRAINTS posts_reply_same_thread DEFERRED`.
DROP TRIGGER posts_check_reply_to ON posts;
DROP FUNCTION posts_check_reply_to();

CREATE INDEX posts_reply_to ON posts (reply_to_post_id) WHERE reply_to_post_id IS NOT NULL;

CREATE FUNCTION posts_check_reply_same_thread() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    stray bigint;
BEGIN
    IF NEW.reply_to_post_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM posts WHERE id = NEW.reply_to_post_id AND thread_id = NEW.thread_id
    ) THEN
        RAISE EXCEPTION 'reply_to_post_id % is not in thread %', NEW.reply_to_post_id, NEW.thread_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF TG_OP = 'UPDATE' AND OLD.thread_id IS DISTINCT FROM NEW.thread_id THEN
        SELECT id INTO stray FROM posts
        WHERE reply_to_post_id = NEW.id AND thread_id <> NEW.thread_id
        LIMIT 1;
        IF FOUND THEN
            RAISE EXCEPTION 'post % replies to post %, which is no longer in its thread', stray, NEW.id
                USING ERRCODE = 'foreign_key_violation';
        END IF;
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER posts_reply_same_thread AFTER INSERT OR UPDATE OF reply_to_post_id, thread_id ON posts
    DEFERRABLE INITIALLY IMMEDIATE
    FOR EACH ROW EXECUTE FUNCTION posts_check_reply_same_thread();
