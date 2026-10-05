-- Break glass, after the v0.2 review: a grant opened by a change of who
-- may read something (moving a thread, a space's read grant, a thread's
-- restriction, an admin joining a site group) is marked as such
-- (DESIGN.md §4, §8; docs/v02-decisions.md #128, #146).
--
-- The member of staff then reads what it opened as one of its readers,
-- for as long as the change stands, and the pages they open aren't
-- recorded one by one; so the logs say that rather than when the grant
-- closes and how many pages were seen under it.
--
-- Adding a column changes no row, so the append-only triggers on
-- staff_access are untouched. Grants opened before this migration keep
-- false: which of them came from a change isn't recorded anywhere that
-- could tell them apart for certain.

ALTER TABLE staff_access ADD COLUMN by_change boolean NOT NULL DEFAULT false;

-- Those who run a space moderate in it (DESIGN.md §3.1, §3.2), and so are
-- shown its hidden posts, marked as hidden, as staff are: on its pages,
-- in search and in the images those posts show. This says whether a
-- space's administer grant lets a member through by more than a role:
-- every member, or a name, group or list of theirs. Reading the space is
-- asked separately, by the thread's own condition. A suspended member is
-- given no viewer id, as everywhere, and runs nothing.
CREATE FUNCTION space_run_by(space_id bigint, viewer_id bigint, viewer_groups bigint[])
RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT viewer_id IS NOT NULL AND EXISTS (
        SELECT 1 FROM space_grants g
        WHERE g.space_id = space_run_by.space_id AND g.verb = 'administer'
          AND (g.level = 'members'
               OR (g.level = 'listed'
                   AND (g.user_ids @> ARRAY[viewer_id] OR g.group_ids && viewer_groups))))
$$;
