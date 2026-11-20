// The table of runs, shared by Observability and a script's page. A run opened from it returns
// to the page it was opened from (`useRunLinkState`).
import type { InfiniteData, UseInfiniteQueryResult } from "@tanstack/react-query";
import { Link, useNavigate } from "react-router";
import type { RunPage, RunSummary } from "../api/types";
import { useRunLinkState } from "../app/runLinks";
import { Ago, Digest, Empty, Loading, ProblemView, RunId, useNow } from "../components/bits";
import { NamespaceName, useNamespaceLabel } from "../components/NamespaceName";
import { ForkTag, QuarantineTag, StatusBadge } from "../components/Status";
import { formatCount, formatDuration, formatRelative } from "../model/format";
import { fieldTerm, queryLink } from "../model/query";
import { RunEnd, RunLabels } from "./RunCells";

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
  const linkState = useRunLinkState();
  const now = useNow(5_000);
  const label = useNamespaceLabel();

  if (query.isPending) return <Loading>Loading runs…</Loading>;
  if (query.isError) return <ProblemView error={query.error} title="The runs could not be read" />;

  const runs = query.data.pages.flatMap((p) => p.runs);
  const showError = runs.some((r) => r.error);
  const showWakes = runs.some((r) => r.wakesAt);
  const open = (id: string) => void navigate(`/runs/${id}`, { state: linkState });
  return (
    <div className="table-wrap">
      <table className="grid runs-table">
        <thead>
          <tr>
            <th>Run</th>
            <th>Status</th>
            {showNamespace && <th title="The end user the run acts for">Namespace</th>}
            {!hideScript && <th>Script</th>}
            <th>Started</th>
            <th className="right">Duration</th>
            <th className="right" title="Tool calls, and those that failed">
              Calls
            </th>
            {showWakes && <th title="When a waiting run wakes by itself">Wakes</th>}
            {showError && (
              <th title="The error the run ended with: hover it for the cause">Error</th>
            )}
          </tr>
        </thead>
        <tbody>
          {runs.map((run) => (
            <tr
              key={run.id}
              className="link"
              onClick={(e) => {
                if ((e.target as HTMLElement).closest("a, button")) return;
                open(run.id);
              }}
            >
              <td className="wrap run-cell">
                <span className="run-cell-id">
                  <Link to={`/runs/${run.id}`} state={linkState} className="row-link">
                    <RunId id={run.id} />
                  </Link>
                  {run.parent && (
                    <Link to={`/runs/${run.parent}`} title={`Forked from ${run.parent}`}>
                      <ForkTag />
                    </Link>
                  )}
                  {run.workflow && (
                    <Link
                      to={queryLink("/observability", fieldTerm("workflow", run.workflow))}
                      title={`A run of the workflow ${run.workflow}${run.startedBy ? `, started by the run ${run.startedBy}` : ""}`}
                    >
                      <span className="tag mono">{run.workflow}</span>
                    </Link>
                  )}
                </span>
                <RunLabels labels={run.labels} onPick={onPickLabel} />
              </td>
              <td>
                <span className="labels">
                  <StatusBadge status={run.status} />
                  {run.quarantined && <QuarantineTag />}
                </span>
              </td>
              {showNamespace && (
                <td>
                  {onPickNamespace ? (
                    <button
                      type="button"
                      className="link-btn ns-cell"
                      title={`Only runs of ${label(run.namespace) ?? run.namespace}`}
                      onClick={() => onPickNamespace(run.namespace)}
                    >
                      <NamespaceName name={run.namespace} />
                    </button>
                  ) : (
                    <span className="ns-cell">
                      <NamespaceName name={run.namespace} />
                    </span>
                  )}
                </td>
              )}
              {!hideScript && (
                <td>
                  {onPickScript ? (
                    <Digest
                      digest={run.script}
                      onPick={() => onPickScript(run.script)}
                      pickTitle="Only runs of this script"
                    />
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
              <td className="right num">
                {formatCount(run.toolCalls)}
                {run.errors > 0 && (
                  <span className="tone-critical" title="Tool calls that failed">
                    {" "}
                    · {formatCount(run.errors)} failed
                  </span>
                )}
              </td>
              {showWakes && (
                <td className="num dim" title={run.wakesAt ?? undefined}>
                  {run.wakesAt ? formatRelative(Date.parse(run.wakesAt), now) : ""}
                </td>
              )}
              {showError && (
                <td className="end-td">
                  <RunEnd run={run} />
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
