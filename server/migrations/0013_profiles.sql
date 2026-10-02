-- The post-info column (DESIGN.md §2): short, single-line, plain-text
-- fields that members edit themselves. The status line is up to 60
-- characters.
ALTER TABLE users
    DROP CONSTRAINT users_status_line_check,
    ADD CONSTRAINT users_title_format CHECK (char_length(title) BETWEEN 1 AND 40 AND title !~ '[[:cntrl:]]'),
    ADD CONSTRAINT users_pronouns_format CHECK (char_length(pronouns) BETWEEN 1 AND 30 AND pronouns !~ '[[:cntrl:]]'),
    ADD CONSTRAINT users_status_line_format CHECK (char_length(status_line) BETWEEN 1 AND 60 AND status_line !~ '[[:cntrl:]]'),
    ADD CONSTRAINT users_location_format CHECK (char_length(location) BETWEEN 1 AND 40 AND location !~ '[[:cntrl:]]');
