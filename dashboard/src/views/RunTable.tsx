// The table of runs, shared by the Runs view and a script's page.
import type { InfiniteData, UseInfiniteQueryResult } from "@tanstack/react-query";
import type { RunPage, RunSummary } from "../api/types";
import { Link, useNavigate } from "../app/namespace";
import {
  Ago,
  Digest,
  Empty,
  LabelList,
  Loading,
  ProblemView,
  RunId,
  useNow,
} from "../components/bits";
import { ForkTag, QuarantineTag, StatusBadge } from "../components/Status";
import { formatCount, formatDuration, formatRelative } from "../model/format";

export interface RunTableProps {
  query: UseInfiniteQueryResult<InfiniteData<RunPage>>;
  onPickLabel?: (pair: string) => void;
  onPickScript?: (digest: string) => void;
  /** Shows each run's namespace, when the views read every namespace. */
  showNamespace?: boolean;
  onPickNamespace?: (namespace: string) => void;
  /** Hides the script column, on a script's own page. */
  hideScript?: boolean;
  empty?: string;
}

function Duration({ run, now }: { run: RunSummary; now: number }) {
  const start = Date.parse(run.startedAt);
  if (run.endedAt)
    return <span className="num">{formatDuration(Date.parse(run.endedAt) - start)}</span>;
  return (
    <span className="num dim" title="Still running">
      {formatDuration(now - start)}…
    </span>
  );
}

export function RunTable({
  query,
  onPickLabel,
  onPickScript,
  showNamespace = false,
  onPickNamespace,
  hideScript = false,
  empty,
}: RunTableProps) {
  const navigate = useNavigate();
  const now = useNow(5_000);

  if (query.isPending) return <Loading>Loading runs…</Loading>;
  if (query.isError) return <ProblemView error={query.error} title="The runs could not be read" />;

  const runs = query.data.pages.flatMap((p) => p.runs);
  const showError = runs.some((r) => r.error);
  const showWakes = runs.some((r) => r.wakesAt);
  return (
    <div className="table-wrap">
      <table className="grid">
        <thead>
          <tr>
            <th>Status</th>
            <th>Run</th>
            {showNamespace && <th title="The end user the run acts for">Namespace</th>}
            <th>Labels</th>
            {!hideScript && <th>Script</th>}
            <th>Started</th>
            <th className="right">Duration</th>
            <th className="right" title="Tool calls">
              Calls
            </th>
            <th className="right" title="Tool calls that failed">
              Errors
            </th>
            <th className="right" title="Journal entries">
              Entries
            </th>
            {showWakes && <th title="When a waiting run wakes by itself">Wakes</th>}
            {showError && <th>Error</th>}
          </tr>
        </thead>
        <tbody>
          {runs.map((run) => (
            <tr
              key={run.id}
              className="link"
              onClick={(e) => {
                if ((e.target as HTMLElement).closest("a, button")) return;
                navigate(`/runs/${run.id}`);
              }}
            >
              <td>
                <span className="labels">
                  <StatusBadge status={run.status} />
                  {run.quarantined && <QuarantineTag />}
                </span>
              </td>
              <td>
                <span className="labels">
                  <Link to={`/runs/${run.id}`} className="row-link">
                    <RunId id={run.id} />
                  </Link>
                  {run.parent && (
                    <Link to={`/runs/${run.parent}`} title={`Forked from ${run.parent}`}>
                      <ForkTag />
                    </Link>
                  )}
                </span>
              </td>
              {showNamespace && (
                <td>
                  {onPickNamespace ? (
                    <button
                      type="button"
                      className="link-btn mono ns-cell"
                      title={`Only runs of ${run.namespace}`}
                      onClick={() => onPickNamespace(run.namespace)}
                    >
                      {run.namespace}
                    </button>
                  ) : (
                    <span className="mono ns-cell">{run.namespace}</span>
                  )}
                </td>
              )}
              <td className="wrap">
                <LabelList labels={run.labels} {...(onPickLabel ? { onPick: onPickLabel } : {})} />
              </td>
              {!hideScript && (
                <td>
                  {onPickScript ? (
                    <button
                      type="button"
                      className="link-btn"
                      title="Only runs of this script"
                      onClick={() => onPickScript(run.script)}
                    >
                      <Digest digest={run.script} />
                    </button>
                  ) : (
                    <Digest digest={run.script} />
                  )}
                </td>
              )}
              <td className="dim">
                <Ago ms={Date.parse(run.startedAt)} />
              </td>
              <td className="right">
                <Duration run={run} now={now} />
              </td>
              <td className="right num">{formatCount(run.toolCalls)}</td>
              <td className={`right num${run.errors > 0 ? " tone-critical" : " muted"}`}>
                {formatCount(run.errors)}
              </td>
              <td className="right num dim">{formatCount(run.historyLen)}</td>
              {showWakes && (
                <td className="num dim" title={run.wakesAt ?? undefined}>
                  {run.wakesAt ? formatRelative(Date.parse(run.wakesAt), now) : ""}
                </td>
              )}
              {showError && (
                <td className="error-cell">
                  {run.error && (
                    <span
                      className="error-text"
                      title={`${run.error}${run.errorMessage ? `: ${run.errorMessage}` : ""}${run.errorLine ? ` (line ${run.errorLine})` : ""}`}
                    >
                      <span className="mono run-error">{run.error}</span>
                      {run.errorMessage && <span className="dim">{run.errorMessage}</span>}
                    </span>
                  )}
                </td>
              )}
            </tr>
          ))}
        </tbody>
      </table>
      {runs.length === 0 && <Empty>{empty ?? "No run matches these filters."}</Empty>}
      {runs.length > 0 && (
        <div className="list-foot">
          <span className="num">
            {formatCount(runs.length)} run{runs.length === 1 ? "" : "s"}
            {query.hasNextPage ? " shown" : ""}
          </span>
          {query.hasNextPage && (
            <button
              type="button"
              className="btn small"
              disabled={query.isFetchingNextPage}
              onClick={() => void query.fetchNextPage()}
            >
              {query.isFetchingNextPage ? "Loading…" : "Load more"}
            </button>
          )}
          {query.isFetching && !query.isFetchingNextPage && (
            <span className="spinner" role="status" aria-label="Refreshing" />
          )}
        </div>
      )}
    </div>
  );
}
