-- Accounts and invites (DESIGN.md §3.6, §4, §7): sessions, single-use
-- login tokens, invites, and the job queue that sends email.

-- Invites form a tree: every user who registered with one records it and
-- the person who made it, and neither can change afterwards.
CREATE TABLE invites (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- SHA-256 of the code in the invite link. The code itself is only ever
    -- shown to the person who made the invite, once.
    code_hash bytea NOT NULL UNIQUE,
    created_by bigint NOT NULL REFERENCES users (id),
    -- For an invite emailed to someone: only that address can use it.
    email citext,
    max_uses integer NOT NULL DEFAULT 1 CHECK (max_uses >= 1),
    uses integer NOT NULL DEFAULT 0 CHECK (uses >= 0 AND uses <= max_uses),
    expires_at timestamptz NOT NULL,
    -- Groups the new member joins. Groups arrive in M5, which adds the
    -- foreign key check and the joining.
    grant_group_ids bigint[] NOT NULL DEFAULT '{}',
    note text NOT NULL DEFAULT '' CHECK (char_length(note) <= 200),
    created_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz,
    CHECK (email IS NULL OR max_uses = 1)
);

CREATE INDEX invites_created_by ON invites (created_by, created_at DESC);

ALTER TABLE users
    ADD COLUMN invite_id bigint REFERENCES invites (id),
    ADD COLUMN last_seen_at timestamptz,
    ADD CONSTRAINT users_username_format CHECK (username ~ '^[A-Za-z0-9_]{2,24}$'),
    ADD CONSTRAINT users_display_name_length CHECK (char_length(display_name) BETWEEN 1 AND 50);

CREATE INDEX users_invited_by ON users (invited_by_user_id, created_at);
CREATE INDEX users_invite ON users (invite_id) WHERE invite_id IS NOT NULL;

-- Who invited whom is a public record and can't be rewritten.
CREATE FUNCTION users_keep_invite_record() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.invited_by_user_id IS DISTINCT FROM OLD.invited_by_user_id
       OR NEW.invite_id IS DISTINCT FROM OLD.invite_id THEN
        RAISE EXCEPTION 'invited_by_user_id and invite_id cannot be changed'
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER users_keep_invite_record BEFORE UPDATE OF invited_by_user_id, invite_id ON users
    FOR EACH ROW EXECUTE FUNCTION users_keep_invite_record();

-- Logged-in sessions. The cookie holds a random token; only its SHA-256 is
-- stored, so a copy of the database can't be used to log in.
CREATE TABLE sessions (
    token_hash bytea PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    ip text,
    user_agent text
);

CREATE INDEX sessions_user ON sessions (user_id);

-- Single-use tokens sent by email: magic links, email verification and
-- password resets. A magic link also has a short code that can be typed
-- into the page where it was requested; `request_hash` identifies that
-- page's request, and `attempts` counts wrong codes.
CREATE TABLE login_tokens (
    token_hash bytea PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose text NOT NULL CHECK (purpose IN ('magic_link', 'verify_email', 'reset')),
    -- For verify_email: the address being verified, which becomes the
    -- account's address once it is.
    email citext,
    request_hash bytea UNIQUE,
    code_hash bytea,
    attempts integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    used_at timestamptz,
    CHECK ((purpose = 'verify_email') = (email IS NOT NULL)),
    CHECK ((request_hash IS NULL) = (code_hash IS NULL))
);

CREATE INDEX login_tokens_user ON login_tokens (user_id, purpose);

-- Background jobs, run in-process (DESIGN.md §9). Workers claim a due job
-- with FOR UPDATE SKIP LOCKED and hold it by setting `locked_until`; a job
-- whose worker died becomes due again when that passes. Finished jobs are
-- deleted; jobs that fail too often keep their row with `failed_at` set.
CREATE TABLE jobs (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind text NOT NULL,
    payload jsonb NOT NULL,
    run_at timestamptz NOT NULL DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 8 CHECK (max_attempts >= 1),
    locked_until timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    failed_at timestamptz
);

CREATE INDEX jobs_due ON jobs (run_at, id) WHERE failed_at IS NULL;

-- Whether an invite can still be used, and if not why not.
CREATE FUNCTION invite_status(revoked_at timestamptz, uses integer, max_uses integer, expires_at timestamptz)
RETURNS text LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT CASE WHEN revoked_at IS NOT NULL THEN 'revoked'
                WHEN uses >= max_uses THEN 'used'
                WHEN expires_at <= now() THEN 'expired'
                ELSE 'valid' END
$$;
