-- An action can affect several members (suspending an invite branch
-- suspends everyone in it), and each of them may contest it once.
DROP INDEX reports_one_appeal_per_action;
CREATE UNIQUE INDEX reports_one_appeal_per_member ON reports (mod_action_id, user_id)
    WHERE kind = 'appeal';
