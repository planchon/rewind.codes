-- Version 10: when each run's summary last changed (`archi/06-storage-and-scale.md`, "Read model";
-- `archi/08-decisions.md`, "Taken for the live dashboard"): the change stream's loop reads the
-- summaries of the tenants it serves stamped since its last pass.

-- Milliseconds of the database's clock, so that every process reads one clock; each write to a
-- summary stamps it at least a millisecond after its previous stamp. The summaries kept are
-- stamped 0: no stream reads that far back, and a time of the journal (`last_activity_at`, which
-- virtual time may put ahead of the clock) would announce them as changed in the future.
ALTER TABLE run_summaries ADD COLUMN changed_at bigint NOT NULL DEFAULT 0;
CREATE INDEX run_summaries_by_change ON run_summaries (tenant, changed_at);
