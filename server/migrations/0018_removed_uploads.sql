-- Images staff have taken down (DESIGN.md §8, §9). Removing an image
-- deletes its records and its file, so nobody can fetch it from any post
-- or as anyone's picture; its hash is kept here so that the same image
-- can't be uploaded again. Re-encoding is deterministic, so uploading the
-- same file gives the same hash.
CREATE TABLE removed_uploads (
    sha256 text PRIMARY KEY CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    removed_by bigint REFERENCES users (id) ON DELETE SET NULL,
    removed_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE mod_actions DROP CONSTRAINT mod_actions_target_kind_check;
ALTER TABLE mod_actions ADD CONSTRAINT mod_actions_target_kind_check
    CHECK (target_kind IN ('post', 'thread', 'user', 'revision', 'report', 'held_post', 'upload'));
