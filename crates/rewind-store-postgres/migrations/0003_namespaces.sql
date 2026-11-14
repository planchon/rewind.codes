-- Namespaces (archi/06-storage-and-scale.md, "Namespaces"): the end user a run belongs to. The
-- runs that existed before namespaces belong to `migrated`.
ALTER TABLE runs ADD COLUMN namespace TEXT NOT NULL DEFAULT 'migrated';
ALTER TABLE run_summaries ADD COLUMN namespace TEXT NOT NULL DEFAULT 'migrated';
