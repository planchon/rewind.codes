// Run: one run, analysed in depth (archi/05). Its header holds its handle, its tags and its
// controls (Forks, the Controls menu beside it, Export run and Export cassette); its sidebar, its
// sections and its details, and no control. Its sections are the debugger (the code with each
// call, timer, signal wait and log next to the line that caused it; the timeline; the time cursor;
// payloads on demand), the history, the logs and the runs it started. Live through the event
// stream while the run goes on. The time cursor steps through the history: the code, its notes and
// the timeline show the run as it was after a step, and hovering a call or a variable shows its
// value then. "Back" returns to the list of Runs the run was opened from.
import { Bug, FileCode, GitBranch, MessageSquareText, ScrollText, Terminal } from "lucide-react";
import { type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, Navigate, useLocation, useParams, useSearchParams } from "react-router";
import { parseRunPath, runBackPath, runPath } from "../../app/runLinks";
import { PageSidebar, useSidebarFolded } from "../../app/sidebar";
import { Empty, Loading, ProblemView, RunHandle, useNow } from "../../components/bits";
import { CodeView, type LineMark } from "../../components/CodeView";
import { PageHeader } from "../../components/PageHeader";
import { OutcomeGlyph } from "../../components/Status";
import { formatCount, formatDuration, formatRelative } from "../../model/format";
import { type Activity, buildRunModel, type RunModel } from "../../model/run";
import { gutterMarks, markMeaning, markSymbol } from "../../model/steps";
import { isEnded, type Run } from "../../model/types";
import { callName, endedRunOf, readRunEnd, startedRuns } from "../../model/workflows";
import { ApiError } from "../../state/errors";
import { useRun, useRunHistory } from "../../state/runs";
import { useWorkflow } from "../../state/workflows";
import { CursorBar, StepPanel, useCursorKeys, useTimeCursor, ValuePopover } from "./Cursor";
import { activityOutcome, activityTitle } from "./describe";
import { HistoryList, Logs } from "./HistoryPanel";
import { Inspector, type Selection } from "./Inspector";
import { RunBanners, RunDetails, RunHeaderControls, RunTags } from "./RunHeader";
import { StartedRuns } from "./StartedRuns";
import { Timeline } from "./Timeline";
import { buildTargets, sourceIndex } from "./targets";

function readSelection(params: URLSearchParams): Selection {
  // `?entry=` is how links of earlier versions named an event.
  const event = params.get("event") ?? params.get("entry");
  if (event !== null && /^\d+$/.test(event)) return { kind: "event", idx: Number(event) };
  const line = params.get("line");
  if (line !== null && /^\d+$/.test(line)) return { kind: "line", line: Number(line) };
  return null;
}

interface NoteGroup {
  key: string;
  activities: Activity[];
}

/** The notes of a line: calls of one action together, a wait with its timeout, logs together. */
export function groupNotes(activities: Activity[]): NoteGroup[] {
  const groups = new Map<string, NoteGroup>();
  const waits = new Set(activities.filter((a) => a.kind === "signal").map((a) => a.idx));
  for (const a of activities) {
    if (a.kind === "timer" && a.timeoutOf !== null && waits.has(a.timeoutOf)) continue;
    const key =
      a.kind === "call"
        ? `call:${a.action}`
        : a.kind === "signal"
          ? endedRunOf(a.name) !== null
            ? "ended"
            : `signal:${a.name}`
          : a.kind === "timer"
            ? `timer:${a.idx}`
            : "log";
    const group = groups.get(key);
    if (group) group.activities.push(a);
    else groups.set(key, { key, activities: [a] });
  }
  return [...groups.values()];
}

const SEVERITY = {
  failed: 4,
  open: 3,
  timed_out: 2,
  cancelled: 1,
  ok: 0,
  fired: 0,
  received: 0,
} as const;

function worst(activities: Activity[], runEnded: boolean) {
  return activities
    .map((a) => activityOutcome(a, runEnded))
    .reduce(
      (w, o) => (SEVERITY[o] > SEVERITY[w] ? o : w),
      "ok" as ReturnType<typeof activityOutcome>,
    );
}

/** The text of a note. Once the run has ended, what was still pending was cancelled. */
function noteText(group: NoteGroup, now: number, ended: boolean): ReactNode {
  const first = group.activities[0];
  if (!first) return null;
  const n = group.activities.length;
  const outcome = (a: Activity) => activityOutcome(a, ended);
  switch (first.kind) {
    case "call": {
      const failed = group.activities.filter((a) => outcome(a) === "failed").length;
      const open = group.activities.filter((a) => outcome(a) === "open").length;
      const cancelled = group.activities.filter((a) => outcome(a) === "cancelled").length;
      if (n === 1) {
        const end = first.end;
        return (
          <>
            {callName(first.action, first.args)}
            <span className="note-meta">
              {end
                ? end.outcome === "failed"
                  ? end.error.kind
                  : formatDuration(end.at - first.at)
                : ended
                  ? "cancelled"
                  : "in flight"}
            </span>
          </>
        );
      }
      return (
        <>
          {callName(first.action, first.args)} <span className="note-meta">×{n}</span>
          {failed > 0 && <span className="note-meta tone-critical">{failed} failed</span>}
          {open > 0 && <span className="note-meta">{open} in flight</span>}
          {cancelled > 0 && <span className="note-meta">{cancelled} cancelled</span>}
        </>
      );
    }
    case "timer":
      return (
        <>
          sleep {formatDuration(first.durationMs)}
          {!first.end && !ended && (
            <span className="note-meta">fires {formatRelative(first.fireAt, now)}</span>
          )}
          {(first.end?.outcome === "cancelled" || (!first.end && ended)) && (
            <span className="note-meta">cancelled</span>
          )}
        </>
      );
    case "signal": {
      if (endedRunOf(first.name) !== null) return endedNote(group, now, ended);
      const end = first.end;
      return (
        <>
          signal "{first.name}"{n > 1 ? <span className="note-meta">×{n}</span> : null}
          <span className="note-meta">
            {end === null && ended
              ? "cancelled"
              : end === null
                ? first.timeoutAt
                  ? `times out ${formatRelative(first.timeoutAt, now)}`
                  : "waiting"
                : end.outcome === "timed_out"
                  ? "timed out"
                  : end.buffered
                    ? "was waiting in the mailbox"
                    : `after ${formatDuration(end.at - first.at)}`}
          </span>
        </>
      );
    }
    case "log":
      return (
        <>
          <MessageSquareText size={11} aria-hidden="true" />
          <span className="note-log">{first.message}</span>
          {n > 1 && <span className="note-meta">+{n - 1}</span>}
        </>
      );
  }
}

/** The waits of `run.result()` on a line: the end of each run, or how long it is awaited. */
function endedNote(group: NoteGroup, now: number, runEnded: boolean): ReactNode {
  const waits = group.activities.filter((a) => a.kind === "signal");
  const first = waits[0];
  if (!first) return null;
  if (waits.length === 1) {
    const end = first.end;
    return (
      <>
        {activityTitle(first)}
        <span className="note-meta">
          {end === null && runEnded
            ? "cancelled"
            : end === null
              ? first.timeoutAt
                ? `times out ${formatRelative(first.timeoutAt, now)}`
                : "waiting"
              : end.outcome === "timed_out"
                ? "timed out"
                : `${readRunEnd(end.payload).status ?? "ended"}${end.buffered ? "" : ` after ${formatDuration(end.at - first.at)}`}`}
        </span>
      </>
    );
  }
  const ended = waits.filter((w) => w.end?.outcome === "received").length;
  const failed = waits.filter((w) => activityOutcome(w, runEnded) === "failed").length;
  return (
    <>
      run.result() <span className="note-meta">×{waits.length}</span>
      <span className="note-meta">
        {ended === waits.length ? "all ended" : `${ended} of ${waits.length} ended`}
      </span>
      {failed > 0 && <span className="note-meta tone-critical">{failed} failed</span>}
    </>
  );
}

function Notes({
  model,
  line,
  selected,
  now,
  onSelectEvent,
  onSelectLine,
}: {
  model: RunModel;
  line: number;
  selected: number | null;
  now: number;
  onSelectEvent: (idx: number) => void;
  onSelectLine: (line: number) => void;
}) {
  const activities = model.byLine.get(line);
  if (!activities) return null;
  return (
    <span className="notes">
      {groupNotes(activities).map((g) => {
        const first = g.activities[0];
        if (!first) return null;
        const outcome = worst(g.activities, model.end !== null);
        const isSelected =
          selected !== null &&
          g.activities.some(
            (a) => a.idx === selected || (a.kind === "signal" && a.end?.idx === selected),
          );
        return (
          <button
            key={g.key}
            type="button"
            className={`note kind-${first.kind} outcome-${outcome}${isSelected ? " selected" : ""}`}
            onClick={() =>
              g.activities.length === 1 ? onSelectEvent(first.idx) : onSelectLine(line)
            }
          >
            {first.kind !== "log" && <OutcomeGlyph outcome={outcome} size={10} />}
            {noteText(g, now, model.end !== null)}
          </button>
        );
      })}
    </span>
  );
}

type Section = "debugger" | "history" | "logs" | "started";

const SECTIONS: readonly Section[] = ["debugger", "history", "logs", "started"];

/** The code a run executes: its workflow version's, read with the version. */
function useRunCode(run: Run | undefined): {
  code: string | null;
  pending: boolean;
  error: unknown;
} {
  const workflow = run?.workflow ?? null;
  const read = useWorkflow(workflow?.id ?? null, workflow?.version ?? null);
  const code =
    read.data && read.data.version.codeDigest === run?.codeDigest ? read.data.code : null;
  const missing = run !== undefined && workflow === null;
  return {
    code,
    pending: !missing && read.isPending,
    error: missing
      ? new Error("The run names no workflow version, whose code it executes.")
      : read.isError
        ? read.error
        : read.data && code === null
          ? new Error("The run's code is not its workflow version's.")
          : null,
  };
}

/** A run's page, mounted again for another run: its details are never the previous run's. */
/**
 * A run's page, by its UUID or its handle (`/runs/dana/triage/06bqx3k9f2mt/history`). A handle
 * is read once into the run's id, which the page and its streams follow; the URL keeps the handle.
 */
export function RunPage() {
  const rest = useParams()["*"] ?? "";
  const { run, section } = parseRunPath(rest);
  const byHandle = run.includes("/");
  const named = useRun(run, byHandle);
  const location = useLocation();
  if (!byHandle) return <RunView key={run} id={run} base={run} section={section} />;
  if (named.isPending) return <Loading>Loading the run…</Loading>;
  if (named.isError) {
    const notFound = named.error instanceof ApiError && named.error.status === 404;
    return (
      <div className="page">
        <ProblemView
          error={named.error}
          title={notFound ? "No such run" : "The run could not be read"}
        />
        <p>
          <Link to={runBackPath(location.state)}>Back to the runs</Link>
        </p>
      </div>
    );
  }
  return <RunView key={named.data.id} id={named.data.id} base={run} section={section} />;
}

function RunView({
  id,
  base,
  section: rawSection,
}: {
  /** The run's UUID. */
  id: string;
  /** How the URL names it: its handle, or its UUID. */
  base: string;
  section: string | null;
}) {
  const section = (rawSection ?? "debugger") as Section | "journal";
  const location = useLocation();
  const folded = useSidebarFolded();
  const back = runBackPath(location.state);
  const [params, setParams] = useSearchParams();
  const run = useRun(id);
  const history = useRunHistory(id);
  const code = useRunCode(run.data);
  const model = useMemo(() => buildRunModel(history.events), [history.events]);
  const live = run.data ? !isEnded(run.data.status) && history.state !== "ended" : false;
  const now = useNow(live ? 5_000 : 60_000);

  // The time cursor: the run as it was after a step. At the head, the run as it is.
  const cursor = useTimeCursor(history.events);
  useCursorKeys(cursor);
  const atCursor = useMemo(
    () => (cursor.atHead ? model : buildRunModel(history.events.slice(0, cursor.cursor + 1))),
    [cursor.atHead, cursor.cursor, history.events, model],
  );
  const index = useMemo(() => (code.code !== null ? sourceIndex(code.code) : null), [code.code]);
  const targets = useMemo(() => (index ? buildTargets(index, model) : null), [index, model]);
  const lastStep = cursor.steps[cursor.steps.length - 1];
  const ending = lastStep?.ends ? { step: cursor.steps.length - 1, kind: lastStep.ends } : null;
  const marks = useMemo(
    () =>
      cursor.atHead || cursor.n < 0
        ? null
        : gutterMarks(model, cursor.steps, cursor.n, targets?.startLine ?? null, null),
    [cursor.atHead, cursor.n, cursor.steps, model, targets],
  );

  // Watches and the value under the pointer.
  const [watches, setWatches] = useState<string[]>([]);
  const [hover, setHover] = useState<{ key: string; rect: DOMRect } | null>(null);
  const hideTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const keepHover = useCallback(() => clearTimeout(hideTimer.current), []);
  const dropHover = useCallback(() => {
    clearTimeout(hideTimer.current);
    hideTimer.current = setTimeout(() => setHover(null), 160);
  }, []);
  useEffect(() => {
    const close = () => setHover(null);
    window.addEventListener("scroll", close, { passive: true });
    return () => {
      window.removeEventListener("scroll", close);
      clearTimeout(hideTimer.current);
    };
  }, []);
  const targetEvents = useMemo(
    () => ({
      enter: (key: string, element: HTMLElement) => {
        clearTimeout(hideTimer.current);
        setHover({ key, rect: element.getBoundingClientRect() });
      },
      leave: dropHover,
      pick: (key: string) =>
        setWatches((w) => (w.includes(key) ? w.filter((k) => k !== key) : [...w, key])),
    }),
    [dropHover],
  );

  const selection = readSelection(params);
  const select = useCallback(
    (next: Selection) => {
      const p = new URLSearchParams(params);
      p.delete("event");
      p.delete("entry");
      p.delete("line");
      if (next?.kind === "event") p.set("event", String(next.idx));
      if (next?.kind === "line") p.set("line", String(next.line));
      setParams(p, { replace: true });
    },
    [params, setParams],
  );
  const selectEvent = useCallback((idx: number) => select({ kind: "event", idx }), [select]);
  const selectLine = useCallback((line: number) => select({ kind: "line", line }), [select]);

  const selectedIdx = selection?.kind === "event" ? selection.idx : null;
  const selectedActivity = selectedIdx !== null ? model.byIdx.get(selectedIdx) : undefined;
  const selectedLine =
    selection?.kind === "line" ? selection.line : (selectedActivity?.callsite?.line ?? null);
  const errorLine = cursor.atHead ? (run.data?.error?.location?.line ?? null) : null;

  const mark = useCallback(
    (line: number): LineMark | null => {
      if (line === errorLine) return "error";
      const activities = atCursor.byLine.get(line);
      if (!activities) return null;
      const outcome = worst(activities, atCursor.end !== null);
      if (outcome === "failed") return "failed";
      if (outcome === "open") return "open";
      return "activity";
    },
    [atCursor, errorLine],
  );

  // The journal of earlier versions is the history.
  if (section === "journal")
    return (
      <Navigate
        to={{ pathname: runPath(base, "history"), search: location.search }}
        state={location.state}
        replace
      />
    );
  if (!SECTIONS.includes(section as Section)) return <Navigate to={runPath(base)} replace />;
  if (run.isPending) return <Loading>Loading the run…</Loading>;
  if (run.isError) {
    const notFound = run.error instanceof ApiError && run.error.status === 404;
    return (
      <div className="page">
        <ProblemView
          error={run.error}
          title={notFound ? "No such run" : "The run could not be read"}
        />
        <p>
          <Link to={back}>Back to the runs</Link>
        </p>
      </div>
    );
  }

  const r = run.data;
  const unattributed = model.unattributed.filter((a) => a.kind !== "timer" || a.timeoutOf === null);
  const ctx = {
    model,
    steps: cursor.steps,
    cursor: cursor.cursor,
    runId: r.id,
    input: model.start?.event.input ?? null,
  };
  const hovered = hover ? targets?.byKey.get(hover.key) : undefined;
  const sectionTo = (to: Section) => ({
    pathname: runPath(base, to === "debugger" ? undefined : to),
    search: params.toString() ? `?${params.toString()}` : "",
  });
  const started = startedRuns(model).length;
  const nav: { section: Section; label: string; icon: ReactNode; end: ReactNode }[] = [
    {
      section: "debugger",
      label: "Debugger",
      icon: <Bug size={16} />,
      end:
        cursor.steps.length > 0 ? (
          <span className="nav-meta num">
            step {cursor.n + 1}/{cursor.steps.length}
          </span>
        ) : null,
    },
    {
      section: "history",
      label: "History",
      icon: <ScrollText size={16} />,
      end: <span className="count">{formatCount(model.entries.length)}</span>,
    },
    {
      section: "logs",
      label: "Logs",
      icon: <Terminal size={16} />,
      end: <span className="count">{formatCount(model.logs.length)}</span>,
    },
    {
      section: "started",
      label: "Runs started",
      icon: <GitBranch size={16} />,
      end: <span className="count">{formatCount(started)}</span>,
    },
  ];
  const inspector = (
    <Inspector
      run={r}
      model={model}
      selection={selection}
      onSelectEvent={selectEvent}
      onSelectLine={selectLine}
      onClear={() => select(null)}
      steps={cursor.steps}
      onGoStep={cursor.go}
    />
  );
  return (
    <div className="page run-page">
      <PageSidebar title="Run" back={back} backLabel="Back to the runs">
        <nav className="nav" aria-label="Run">
          {nav.map((n) => (
            <Link
              key={n.section}
              to={sectionTo(n.section)}
              state={location.state}
              className="nav-item"
              title={folded ? n.label : undefined}
              aria-current={section === n.section ? "page" : undefined}
            >
              <span className="nav-icon" aria-hidden="true">
                {n.icon}
              </span>
              <span className="nav-label">{n.label}</span>
              {n.end && <span className="nav-end">{n.end}</span>}
            </Link>
          ))}
        </nav>
        <section className="pane-section" aria-label="Details">
          <h2 className="pane-label">Details</h2>
          <RunDetails run={r} model={model} />
        </section>
      </PageSidebar>
      <PageHeader
        crumbs={<Link to={back}>Runs</Link>}
        title={<RunHandle handle={r.handle} id={r.id} />}
        meta={<RunTags run={r} />}
        actions={<RunHeaderControls run={r} />}
      />
      <div className="page-body">
        {history.state === "error" && (
          <ProblemView error={history.error} title="The history could not be read" />
        )}
        <RunBanners run={r} model={model} onSelectEvent={selectEvent} onSelectLine={selectLine} />

        {section === "debugger" && (
          <>
            <CursorBar cursor={cursor} live={live} ending={ending} t0={model.firstAt} />
            <section className="panel" aria-label="Timeline">
              <header className="panel-head">
                <h2>Timeline</h2>
                {model.firstAt !== null && model.lastAt !== null && (
                  <span className="muted num">
                    {formatDuration((live ? now : model.lastAt) - model.firstAt)} from the first
                    event
                    {live ? " to now" : " to the last"}
                  </span>
                )}
              </header>
              {history.events.length === 0 && history.state !== "ended" ? (
                <Loading>Reading the history…</Loading>
              ) : (
                <Timeline
                  model={model}
                  now={live ? now : null}
                  selected={selectedIdx}
                  onSelect={selectEvent}
                  cursor={{
                    steps: cursor.steps,
                    n: cursor.n,
                    last: cursor.cursor,
                    atHead: cursor.atHead,
                    onSeek: cursor.go,
                  }}
                />
              )}
            </section>

            <section className="panel source-panel" aria-label="Code">
              <header className="panel-head">
                <h2>
                  <FileCode size={14} aria-hidden="true" /> workflow.ts
                </h2>
                {marks ? (
                  <span className="mark-legend muted">
                    <span>
                      <b className="mk resumed">▸</b> resumed
                    </span>
                    <span>
                      <b className="mk issued">①</b> issued
                    </span>
                    <span>
                      <b className="mk paused">‖</b> paused
                    </span>
                    <span>
                      <b className="mk done">✓</b> done
                    </span>
                  </span>
                ) : (
                  <span className="muted">each line with what it caused</span>
                )}
                {unattributed.length > 0 && (
                  <span className="end muted" title="Activities recorded without a callsite">
                    {unattributed.length} without a line
                  </span>
                )}
              </header>
              {code.pending ? (
                <Loading>Loading the code…</Loading>
              ) : code.code === null ? (
                <div className="panel-body">
                  <ProblemView error={code.error} title="The code could not be read" />
                </div>
              ) : (
                <CodeView
                  source={code.code}
                  notesTitle="What each line caused"
                  notes={(line) => (
                    <Notes
                      model={atCursor}
                      line={line}
                      selected={selectedIdx}
                      now={now}
                      onSelectEvent={selectEvent}
                      onSelectLine={selectLine}
                    />
                  )}
                  mark={mark}
                  gutter={
                    marks
                      ? (line) => {
                          const m = marks.get(line);
                          return m ? (
                            <span className={`mk ${m.kind}`} title={markMeaning(m)}>
                              {markSymbol(m)}
                            </span>
                          ) : (
                            <span className="mk" />
                          );
                        }
                      : undefined
                  }
                  targets={targets ? (line) => targets.byLine.get(line) ?? [] : undefined}
                  targetClass={(key) =>
                    `${watches.includes(key) ? "pinned" : ""}${hover?.key === key ? " hovered" : ""}`.trim()
                  }
                  targetEvents={targets ? targetEvents : undefined}
                  selectedLine={selectedLine}
                  focusLine={selectedLine}
                  onLineClick={selectLine}
                />
              )}
            </section>
            <div className="debug-below">
              <StepPanel
                cursor={cursor}
                model={model}
                watches={watches}
                targets={targets?.byKey ?? new Map()}
                ctx={ctx}
                onUnwatch={(key) => setWatches((w) => w.filter((k) => k !== key))}
                onSelectLine={selectLine}
                onSelectEvent={selectEvent}
              />
              {inspector}
            </div>
            {hover && hovered && (
              <ValuePopover
                target={hovered}
                anchor={hover.rect}
                ctx={ctx}
                pinned={watches.includes(hover.key)}
                onEnter={keepHover}
                onLeave={dropHover}
              />
            )}
          </>
        )}

        {section === "history" && (
          <div className="run-body">
            <section className="panel" aria-label="History">
              <header className="panel-head">
                <h2>History</h2>
                <span className="muted">lifecycle · commands · completions · annotations</span>
              </header>
              <HistoryList model={model} selected={selectedIdx} onSelect={selectEvent} />
            </section>
            <div className="run-side">{inspector}</div>
          </div>
        )}

        {section === "logs" && (
          <div className="run-body">
            <section className="panel" aria-label="Logs">
              <header className="panel-head">
                <h2>Logs</h2>
                <span className="muted">what its code wrote with console</span>
              </header>
              <Logs model={model} onSelect={selectEvent} onSelectLine={selectLine} />
            </section>
            <div className="run-side">{inspector}</div>
          </div>
        )}

        {section === "started" &&
          (started === 0 ? (
            <Empty>This run started no other run.</Empty>
          ) : (
            <div className="run-body">
              <StartedRuns
                runId={r.id}
                model={model}
                live={live}
                onSelectEvent={selectEvent}
                onSelectLine={selectLine}
              />
              <div className="run-side">{inspector}</div>
            </div>
          ))}
      </div>
    </div>
  );
}
