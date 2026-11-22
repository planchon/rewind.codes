// The table of runs, shared by Observability, a namespace's page and a script's page. With
// `controls`, each header is a menu that orders the runs by its column and filters what it
// shows (RunColumns.tsx). A run opened from it returns to the page it was opened from
// (`useRunLinkState`).
//
// It shows a search of `state/` (archi/05, "Nothing moves on its own"): rows keyed by run, each
// rendered again only when its run changes; cells of one size; the previous search's rows, dimmed,
// while a new one loads; an answer that adds, drops or reorders rows held behind "N new · Show"
// while the table is scrolled.
import { memo, useMemo, useRef, useState } from "react";
import { Link, useNavigate } from "react-router";
import { useRunLinkState } from "../app/runLinks";
import { Ago, Digest, Empty, Loading, ProblemView, RunId, useNow } from "../components/bits";
import { NamespaceName, useNamespaceLabel } from "../components/NamespaceName";
import { ForkTag, QuarantineTag, StatusBadge } from "../components/Status";
import { formatCount, formatDuration, formatRelative } from "../model/format";
import { countTerms, fieldTerm, queryLink } from "../model/query";
import type { RunSummary } from "../model/types";
import type { RunSearchView } from "../state/runs";
import { RunEnd, RunLabels } from "./RunCells";
import {
  CallsHeader,
  DurationHeader,
  ErrorHeader,
  NamespaceHeader,
  RunHeader,
  type RunTableControls,
  ScriptHeader,
  StartedHeader,
  StatusHeader,
  WakesHeader,
} from "./RunColumns";
import { TableToolbar } from "./TableTop";

export type { RunTableControls } from "./RunColumns";

export interface RunTableProps {
  search: RunSearchView;
  onPickLabel?: (pair: string) => void;
  onPickScript?: (digest: string) => void;
  /** Shows each run's namespace, when the views read every namespace. */
  showNamespace?: boolean;
  onPickNamespace?: (namespace: string) => void;
  /** Hides the script column, on a script's own page. */
  hideScript?: boolean;
  /** Headers that order and filter the runs. */
  controls?: RunTableControls;
  empty?: string;
}

/** A plain header, for a table whose order and filters are fixed. */
function Plain({
  title,
  right,
  hint,
  className,
}: {
  title: string;
  right?: boolean;
  hint?: string;
  className: string;
}) {
  return (
    <th className={right ? `${className} right` : className} title={hint}>
      {title}
    </th>
  );
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

/**
 * A column shown once stays while the search is the same: a run that fails live does not add a
 * column, then take it away.
 */
function useSticky(flag: boolean, resetKey: string): boolean {
  const [state, setState] = useState({ key: resetKey, shown: flag });
  if (state.key !== resetKey) {
    setState({ key: resetKey, shown: flag });
    return flag;
  }
  if (flag && !state.shown) {
    setState({ key: resetKey, shown: true });
    return true;
  }
  return state.shown || flag;
}

interface RowHandlers {
  open: (id: string) => void;
  pickLabel: ((pair: string) => void) | undefined;
  pickScript: ((digest: string) => void) | undefined;
  pickNamespace: ((namespace: string) => void) | undefined;
}

interface RowProps {
  run: RunSummary;
  /** The time durations run to: 0 for a run that ended, whose row the clock does not touch. */
  now: number;
  showNamespace: boolean;
  hideScript: boolean;
  showWakes: boolean;
  showError: boolean;
  linkState: { from: string } | undefined;
  handlers: RowHandlers;
}

/** One run: rendered again only when the run, or a column, changes. */
const RunRow = memo(function RunRow({
  run,
  now,
  showNamespace,
  hideScript,
  showWakes,
  showError,
  linkState,
  handlers,
}: RowProps) {
  const label = useNamespaceLabel();
  return (
    <tr
      className="link"
      data-run={run.id}
      onClick={(e) => {
        if ((e.target as HTMLElement).closest("a, button")) return;
        handlers.open(run.id);
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
        <RunLabels labels={run.labels} onPick={handlers.pickLabel} />
      </td>
      <td>
        <span className="labels">
          <StatusBadge status={run.status} />
          {run.quarantined && <QuarantineTag />}
        </span>
      </td>
      {showNamespace && (
        <td>
          {handlers.pickNamespace ? (
            <button
              type="button"
              className="link-btn ns-cell"
              title={`Only runs of ${label(run.namespace) ?? run.namespace}`}
              onClick={() => handlers.pickNamespace?.(run.namespace)}
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
          {handlers.pickScript ? (
            <Digest
              digest={run.script}
              onPick={() => handlers.pickScript?.(run.script)}
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
          {run.wakesAt ? formatRelative(Date.parse(run.wakesAt), now || Date.now()) : ""}
        </td>
      )}
      {showError && (
        <td className="end-td">
          <RunEnd run={run} />
        </td>
      )}
    </tr>
  );
});

export function RunTable({
  search,
  onPickLabel,
  onPickScript,
  showNamespace = false,
  onPickNamespace,
  hideScript = false,
  controls,
  empty,
}: RunTableProps) {
  const navigate = useNavigate();
  const from = useRunLinkState()?.from;
  const linkState = useMemo(() => (from ? { from } : undefined), [from]);
  const now = useNow(5_000);

  // The rows get handlers that never change, which call the latest ones.
  const latest = useRef({ onPickLabel, onPickScript, onPickNamespace, navigate, linkState });
  latest.current = { onPickLabel, onPickScript, onPickNamespace, navigate, linkState };
  const hasLabel = Boolean(onPickLabel);
  const hasScript = Boolean(onPickScript);
  const hasNamespace = Boolean(onPickNamespace);
  const handlers = useMemo<RowHandlers>(
    () => ({
      open: (id) =>
        void latest.current.navigate(`/runs/${id}`, { state: latest.current.linkState }),
      pickLabel: hasLabel ? (pair) => latest.current.onPickLabel?.(pair) : undefined,
      pickScript: hasScript ? (digest) => latest.current.onPickScript?.(digest) : undefined,
      pickNamespace: hasNamespace
        ? (namespace) => latest.current.onPickNamespace?.(namespace)
        : undefined,
    }),
    [hasLabel, hasScript, hasNamespace],
  );

  const runs = search.runs;
  // A column the query filters or orders by stays, to change it, even when no run fills it.
  const asked = (fields: string[]) => (controls ? countTerms(controls.q, fields) > 0 : false);
  const columnsKey = `${controls?.q ?? ""}|${controls?.sort ?? ""}`;
  const showError = useSticky(
    runs.some((r) => r.error) || asked(["error", "fingerprint"]),
    columnsKey,
  );
  const showWakes = useSticky(
    runs.some((r) => r.wakesAt) || controls?.sort === "wakes" || asked(["waiting", "signal"]),
    columnsKey,
  );

  if (search.status === "pending") return <Loading>Loading runs…</Loading>;
  if (search.status === "error")
    return <ProblemView error={search.error} title="The runs could not be read" />;

  return (
    <div className="run-table">
      <TableToolbar pending={search.pending} onShow={search.showPending} setAtTop={search.setAtTop}>
        {runs.length > 0 && (
          <span className="num muted">
            {formatCount(runs.length)} run{runs.length === 1 ? "" : "s"}
            {search.hasMore ? " shown" : ""}
          </span>
        )}
      </TableToolbar>
      <div
        className={`table-wrap${search.stale ? " is-stale" : ""}`}
        aria-busy={search.stale || undefined}
      >
        <table className="grid runs-table">
          <thead>
            {controls ? (
              <tr>
                <RunHeader controls={controls} />
                <StatusHeader controls={controls} />
                {showNamespace && <NamespaceHeader controls={controls} />}
                {!hideScript && <ScriptHeader controls={controls} />}
                <StartedHeader controls={controls} />
                <DurationHeader controls={controls} />
                <CallsHeader controls={controls} />
                {showWakes && <WakesHeader controls={controls} />}
                {showError && <ErrorHeader controls={controls} />}
              </tr>
            ) : (
              <tr>
                <Plain title="Run" className="col-run" />
                <Plain title="Status" className="col-status" />
                {showNamespace && (
                  <Plain
                    title="Namespace"
                    hint="The end user the run acts for"
                    className="col-namespace"
                  />
                )}
                {!hideScript && <Plain title="Script" className="col-script" />}
                <Plain title="Started" className="col-started" />
                <Plain title="Duration" right className="col-duration" />
                <Plain
                  title="Calls"
                  right
                  hint="Tool calls, and those that failed"
                  className="col-calls"
                />
                {showWakes && (
                  <Plain
                    title="Wakes"
                    hint="When a waiting run wakes by itself"
                    className="col-wakes"
                  />
                )}
                {showError && (
                  <Plain
                    title="Error"
                    hint="The error the run ended with: hover it for the cause"
                    className="col-error"
                  />
                )}
              </tr>
            )}
          </thead>
          <tbody>
            {runs.map((run) => (
              <RunRow
                key={run.id}
                run={run}
                now={run.endedAt ? 0 : now}
                showNamespace={showNamespace}
                hideScript={hideScript}
                showWakes={showWakes}
                showError={showError}
                linkState={linkState}
                handlers={handlers}
              />
            ))}
          </tbody>
        </table>
        {runs.length === 0 && <Empty>{empty ?? "No run matches these filters."}</Empty>}
        {search.hasMore && (
          <div className="list-foot">
            <button
              type="button"
              className="btn small"
              disabled={search.loadingMore}
              onClick={search.loadMore}
            >
              {search.loadingMore ? "Loading…" : "Load more"}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
