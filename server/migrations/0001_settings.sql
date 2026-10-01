-- Site-wide settings edited through the admin UI. Values are JSON so that
-- each setting can have whatever shape it needs.
CREATE TABLE settings (
    key text PRIMARY KEY,
    value jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
