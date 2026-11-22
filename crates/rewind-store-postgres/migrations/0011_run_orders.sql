-- Version 9: the orders of a search by duration and by tool calls (`archi/06-storage-and-scale.md`,
-- "Read model").

-- The longest ended runs first, then the newest; the runs that have not ended last. The
-- expression is `DURATION` of `read_model/query.rs`, which the order names as written here.
CREATE INDEX run_summaries_by_duration ON run_summaries (
    tenant,
    (CASE WHEN ended_at >= started_at THEN ended_at - started_at
        WHEN ended_at < started_at THEN 0 END) DESC NULLS LAST,
    run_id DESC
);
-- The runs with the most tool calls first, read backwards.
CREATE INDEX run_summaries_by_calls ON run_summaries (tenant, tool_calls, run_id);
