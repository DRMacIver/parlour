-- Thread slugs appear unescaped in canonical URLs and redirects, so they
-- must have the same URL-safe form as category slugs. `store::slugify`
-- only produces this form.
ALTER TABLE threads ADD CONSTRAINT threads_slug_format CHECK (slug ~ '^[a-z0-9]+(-[a-z0-9]+)*$');
