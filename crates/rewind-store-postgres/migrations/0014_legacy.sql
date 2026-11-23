-- Version 14: the tables of runs set aside (`archi/08-decisions.md`, "Taken while writing the
-- object-storage spec": the migration). Runs live in the partitions of the run plane and in the
-- archive; the database holds the control plane only (`archi/06-storage-and-scale.md`, "Where
-- data lives").
--
-- The tables the journal store kept its runs in move to the schema `legacy`, with
-- their indexes, constraints, sequences and statistics: the runs, their histories, inboxes,
-- signal keys, timers, tool tasks, webhook tokens and watchers, the payloads and their
-- references, the feed, the read model's summaries, labels, waits, search text, quarantine and
-- calls, and the namespaces' use per day, quota alerts and script counts. `rewind server
-- migrate-runs` reads them there, moves each run into its namespace's partition or into the
-- archive, and drops the schema once every run moved. Nothing else reads them.

CREATE SCHEMA legacy;

ALTER TABLE runs SET SCHEMA legacy;
ALTER TABLE history SET SCHEMA legacy;
ALTER TABLE inbox SET SCHEMA legacy;
ALTER TABLE signal_keys SET SCHEMA legacy;
ALTER TABLE timers SET SCHEMA legacy;
ALTER TABLE tool_tasks SET SCHEMA legacy;
ALTER TABLE webhook_tokens SET SCHEMA legacy;
ALTER TABLE run_watchers SET SCHEMA legacy;
ALTER TABLE payloads SET SCHEMA legacy;
ALTER TABLE payload_refs SET SCHEMA legacy;
ALTER TABLE feed SET SCHEMA legacy;
ALTER TABLE run_summaries SET SCHEMA legacy;
ALTER TABLE run_labels SET SCHEMA legacy;
ALTER TABLE run_waits SET SCHEMA legacy;
ALTER TABLE run_search SET SCHEMA legacy;
ALTER TABLE run_quarantine SET SCHEMA legacy;
ALTER TABLE tool_calls SET SCHEMA legacy;
ALTER TABLE usage SET SCHEMA legacy;
ALTER TABLE quota_alerts SET SCHEMA legacy;
ALTER TABLE namespace_scripts SET SCHEMA legacy;

ALTER STATISTICS run_labels_per_tenant SET SCHEMA legacy;
ALTER STATISTICS tool_calls_per_tenant SET SCHEMA legacy;

ALTER TABLE tenants
    DROP COLUMN journal_retention_ms,
    DROP COLUMN read_model_retention_ms,
    DROP COLUMN payload_grace_ms;
ALTER TABLE tenants RENAME COLUMN archive_retention_ms TO retention_ms;
