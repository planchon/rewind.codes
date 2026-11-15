-- Version 4: the workflow metadata of runs in the read model (`archi/05-testing-and-observability.md`,
-- "Read model" and "Query language": `workflow:triage`, `started_by:<run id>`).

-- For a run of a workflow, the workflow's name; for a run a script started, that run. Like the
-- labels and the parent, the metadata of the latest batch projected.
ALTER TABLE run_summaries
    ADD COLUMN workflow text COLLATE "C",
    ADD COLUMN started_by uuid;
-- Predicates: the workflow, by name or prefix (the "C" collation lets `LIKE 'prefix%'` use the
-- index); the runs a run started.
CREATE INDEX run_summaries_by_workflow ON run_summaries (tenant, workflow)
    WHERE workflow IS NOT NULL;
CREATE INDEX run_summaries_by_started_by ON run_summaries (tenant, started_by)
    WHERE started_by IS NOT NULL;
