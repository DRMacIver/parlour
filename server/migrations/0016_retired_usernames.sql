-- Usernames that can't be used again.
--
-- A deleted account is renamed `deleted_<id>`, so live accounts can't have
-- names starting `deleted_`: one could pass as a deleted member, and would
-- stop that member's account from being renamed when they delete it.
ALTER TABLE users
    ADD CONSTRAINT users_deleted_prefix CHECK (deleted_at IS NOT NULL OR username::text !~* '^deleted_');

-- The usernames members had before deleting their accounts. Other people's
-- posts still mention and quote them by these names, linking to their
-- profiles, so nobody new may take one and pass as them.
--
-- Only a hash of each name is kept, so the table can't be read as a list of
-- who has left: it answers only "is this name retired?", which trying to
-- register the name answers anyway.
CREATE TABLE retired_usernames (
    username_hash bytea PRIMARY KEY,
    retired_at timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION username_hash(name text) RETURNS bytea LANGUAGE sql IMMUTABLE STRICT
    RETURN sha256(convert_to(lower(name), 'UTF8'));

CREATE FUNCTION users_refuse_retired_usernames() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM retired_usernames WHERE username_hash = username_hash(NEW.username::text)) THEN
        RAISE EXCEPTION 'that username belonged to a deleted account'
            USING ERRCODE = 'unique_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER users_refuse_retired_usernames BEFORE INSERT OR UPDATE OF username ON users
    FOR EACH ROW EXECUTE FUNCTION users_refuse_retired_usernames();
