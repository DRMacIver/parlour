-- Serialise thread moves with changes to a category's read level.
--
-- Without a lock, a transaction moving a thread into category C could read
-- C's old read level while another transaction tightened C; the tightening
-- transaction's propagation couldn't see the uncommitted move either, so the
-- thread kept the old (more visible) level. Locking C's row FOR SHARE here
-- conflicts with the FOR NO KEY UPDATE lock an UPDATE of read_level takes,
-- so one of the two waits for the other and then sees its result.
CREATE OR REPLACE FUNCTION threads_set_visibility() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.category_id IS NULL THEN
        NEW.visibility := 'private';
    ELSE
        SELECT read_level INTO NEW.visibility FROM categories WHERE id = NEW.category_id FOR SHARE;
    END IF;
    RETURN NEW;
END;
$$;
