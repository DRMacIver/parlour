-- Registrations with an invite link that are waiting for the new member to
-- confirm their email address (DESIGN.md §3.6). A link invite can be passed
-- around, so the account is only made once the email sent to the address
-- typed has been followed. Until then nothing says whether the address
-- belongs to a member already (that member is emailed instead).
CREATE TABLE registrations (
    -- SHA-256 of the token in the emailed link.
    token_hash bytea PRIMARY KEY,
    invite_id bigint NOT NULL REFERENCES invites (id),
    username citext NOT NULL,
    display_name text NOT NULL,
    email citext NOT NULL,
    password_hash text,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL
);

CREATE INDEX registrations_expires ON registrations (expires_at);
