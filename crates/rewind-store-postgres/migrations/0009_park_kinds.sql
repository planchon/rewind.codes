-- Version 9: why a tool task is parked, since runs attach their secrets (`archi/04-tools.md`,
-- "Tool task lifecycle"): a call its server refused (`credentials_refused`, which a 401 parked
-- as `credentials_expired` before) and a secret the run attached that no longer exists
-- (`secret_missing`). A task parked before keeps `credentials_expired`, read as a refusal.
ALTER TABLE tool_tasks DROP CONSTRAINT tool_tasks_parked_kind_check;
ALTER TABLE tool_tasks ADD CONSTRAINT tool_tasks_parked_kind_check CHECK (
    parked_kind IN ('schema_changed', 'credentials_refused', 'secret_missing', 'credentials_expired')
);
