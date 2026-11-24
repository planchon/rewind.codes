// The table of runs, shared by Observability, a namespace's page and a script's page. With
// `controls`, each header is a menu that orders the runs by its column and filters what it
// shows (RunColumns.tsx). A run opened from it returns to the page it was opened from
// (`useRunLinkState`).
//
// It shows a search of `state/` (archi/05, "Nothing moves on its own"): rows keyed by run, each
// rendered again only when its run changes; columns at least their widths, no cell cut; the
// previous search's rows, dimmed,
// while a new one loads; an answer that adds, drops or reorders rows held behind "N new · Show"
// while the table is scrolled.
import { memo, useMemo, useRef, useState } from "react";
import { Link, useNavigate } from "react-router";
import { runPath, useRunLinkState } from "../app/runLinks";
import { Ago, Empty, Loading, ProblemView, RunHandle, useNow } from "../components/bits";
import { NamespaceName, useNamespaceLabel } from "../components/NamespaceName";
import { ForkTag, QuarantineTag, StatusBadge } from "../components/Status";
import { TableScroll } from "../components/TableScroll";
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
import { TableTop } from "./TableTop";

export type { RunTableControls } from "./RunColumns";

/**
 * The least widths of the columns, in px: each is wider when its rows hold more, nothing in a
 * cell being cut. The run's column, and the error's when it shows, take what the table has left.
 * A table wider than its page scrolls sideways (TableScroll.tsx), its header still following the
 * page.
 */
const WIDTHS = {
  status: 120,
  namespace: 140,
  script: 144,
  started: 96,
  duration: 100,
  calls: 104,
  wakes: 100,
} as const;

type Column = keyof typeof WIDTHS;

/** The optional columns a row shows. */
interface Cols {
  namespace: boolean;
  script: boolean;
  started: boolean;
  duration: boolean;
  calls: boolean;
  wakes: boolean;
  error: boolean;
}

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

/** What a run ran, by name: its script's title, else its workflow, else "Untitled script". */
function scriptName(run: RunSummary): string {
  return run.scriptTitle ?? run.workflow ?? "Untitled script";
}

function Duration({ run, now }: { run: RunSummary; now: number }) {
  const start = Date.parse(run.startedAt);
  if (run.endedAt)
    return <span className="num">{formatDuration(Date.parse(run.endedAt) - start)}</span>;
  return (
    <span className="num dim" title="Still running">
      {formatDuration(now - start)}
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
  cols: Cols;
  linkState: { from: string } | undefined;
  handlers: RowHandlers;
}

/** One run: rendered again only when the run, or a column, changes. */
const RunRow = memo(function RunRow({ run, now, cols, linkState, handlers }: RowProps) {
  const label = useNamespaceLabel();
  return (
    <tr
      className="link"
      data-run={run.id}
      onClick={(e) => {
        if ((e.target as HTMLElement).closest("a, button")) return;
        handlers.open(run.handle);
      }}
    >
      <td className="wrap run-cell">
        <span className="run-cell-id">
          <Link to={runPath(run.handle)} state={linkState} className="row-link">
            <RunHandle
              handle={run.handle}
              id={run.id}
              namespace={!cols.namespace}
              copy={false}
              whole
            />
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
      {cols.namespace && (
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
      {cols.script && (
        <td>
          {handlers.pickScript ? (
            <button
              type="button"
              className="link-btn script-cell-title"
              title={`Only runs of this script · ${run.script}`}
              onClick={() => handlers.pickScript?.(run.script)}
            >
              {scriptName(run)}
            </button>
          ) : (
            <span className="script-cell-title" title={run.script}>
              {scriptName(run)}
            </span>
          )}
        </td>
      )}
      {cols.started && (
        <td className="dim">
          <Ago ms={Date.parse(run.startedAt)} />
        </td>
      )}
      {cols.duration && (
        <td className="right">
          <Duration run={run} now={now} />
        </td>
      )}
      {cols.calls && (
        <td className="right num">
          {formatCount(run.toolCalls)}
          {run.errors > 0 && (
            <span className="tone-critical" title="Tool calls that failed">
              {" "}
              · {formatCount(run.errors)} failed
            </span>
          )}
        </td>
      )}
      {cols.wakes && (
        <td className="num dim" title={run.wakesAt ?? undefined}>
          {run.wakesAt ? formatRelative(Date.parse(run.wakesAt), now || Date.now()) : ""}
        </td>
      )}
      {cols.error && (
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
      open: (handle) =>
        void latest.current.navigate(runPath(handle), { state: latest.current.linkState }),
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

  const shown: Column[] = [
    "status",
    ...(showNamespace ? (["namespace"] as const) : []),
    ...(hideScript ? [] : (["script"] as const)),
    "started",
    "duration",
    "calls",
    ...(showWakes ? (["wakes"] as const) : []),
  ];
  // One object while the same columns show, so the rows are not rendered again for nothing.
  const colsKey = [...shown, ...(showError ? ["error"] : [])].join(",");
  const cols = useMemo<Cols>(() => {
    const on = new Set(colsKey.split(","));
    return {
      namespace: on.has("namespace"),
      script: on.has("script"),
      started: on.has("started"),
      duration: on.has("duration"),
      calls: on.has("calls"),
      wakes: on.has("wakes"),
      error: on.has("error"),
    };
  }, [colsKey]);

  if (search.status === "pending") return <Loading>Loading runs…</Loading>;
  if (search.status === "error")
    return <ProblemView error={search.error} title="The runs could not be read" />;

  return (
    <div className="run-table">
      <TableTop pending={search.pending} onShow={search.showPending} setAtTop={search.setAtTop} />
      <TableScroll stale={search.stale}>
        <table className="grid runs-table">
          <colgroup>
            <col className="col-run" />
            {shown.map((c) => (
              <col key={c} className={`col-${c}`} style={{ width: WIDTHS[c] }} />
            ))}
            {showError && <col className="col-error" />}
          </colgroup>
          <thead>
            {controls ? (
              <tr>
                <RunHeader controls={controls} />
                <StatusHeader controls={controls} />
                {cols.namespace && <NamespaceHeader controls={controls} />}
                {cols.script && <ScriptHeader controls={controls} />}
                {cols.started && <StartedHeader controls={controls} />}
                {cols.duration && <DurationHeader controls={controls} />}
                {cols.calls && <CallsHeader controls={controls} />}
                {cols.wakes && <WakesHeader controls={controls} />}
                {cols.error && <ErrorHeader controls={controls} />}
              </tr>
            ) : (
              <tr>
                <Plain title="Run" className="col-run" />
                <Plain title="Status" className="col-status" />
                {cols.namespace && (
                  <Plain
                    title="Namespace"
                    hint="The end user the run acts for"
                    className="col-namespace"
                  />
                )}
                {cols.script && <Plain title="Script" className="col-script" />}
                {cols.started && <Plain title="Started" className="col-started" />}
                {cols.duration && <Plain title="Duration" right className="col-duration" />}
                {cols.calls && (
                  <Plain
                    title="Calls"
                    right
                    hint="Tool calls, and those that failed"
                    className="col-calls"
                  />
                )}
                {cols.wakes && (
                  <Plain
                    title="Wakes"
                    hint="When a waiting run wakes by itself"
                    className="col-wakes"
                  />
                )}
                {cols.error && (
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
                cols={cols}
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
      </TableScroll>
    </div>
  );
}
