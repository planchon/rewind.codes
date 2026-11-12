// Run: the source with each tool call, timer, signal wait and log next to the line that caused
// it; the timeline; the journal; payloads on demand. Live through the event stream while the run
// goes on.
import { FileCode, MessageSquareText } from "lucide-react";
import { type ReactNode, useCallback, useMemo } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { ApiError } from "../../api/client";
import { useRun, useRunJournal, useScript } from "../../api/queries";
import { isEnded } from "../../api/types";
import { Loading, ProblemView, useNow } from "../../components/bits";
import { CodeView, type LineMark } from "../../components/CodeView";
import { OutcomeGlyph } from "../../components/Status";
import { formatDuration, formatRelative } from "../../model/format";
import { type Activity, buildRunModel, type RunModel } from "../../model/run";
import { activityOutcome } from "./describe";
import { Inspector, type Selection } from "./Inspector";
import { JournalPanel } from "./JournalPanel";
import { RunBanners, RunHeader } from "./RunHeader";
import { Timeline } from "./Timeline";

function readSelection(params: URLSearchParams): Selection {
  const entry = params.get("entry");
  if (entry !== null && /^\d+$/.test(entry)) return { kind: "entry", idx: Number(entry) };
  const line = params.get("line");
  if (line !== null && /^\d+$/.test(line)) return { kind: "line", line: Number(line) };
  return null;
}

interface NoteGroup {
  key: string;
  activities: Activity[];
}

/** The notes of a line: calls of one tool together, a wait with its timeout, logs together. */
export function groupNotes(activities: Activity[]): NoteGroup[] {
  const groups = new Map<string, NoteGroup>();
  const waits = new Set(activities.filter((a) => a.kind === "signal").map((a) => a.idx));
  for (const a of activities) {
    if (a.kind === "timer" && a.timeoutOf !== null && waits.has(a.timeoutOf)) continue;
    const key =
      a.kind === "tool_call"
        ? `call:${a.tool}`
        : a.kind === "signal"
          ? `signal:${a.name}`
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

function worst(activities: Activity[]) {
  return activities
    .map(activityOutcome)
    .reduce(
      (w, o) => (SEVERITY[o] > SEVERITY[w] ? o : w),
      "ok" as ReturnType<typeof activityOutcome>,
    );
}

function noteText(group: NoteGroup, now: number): ReactNode {
  const first = group.activities[0];
  if (!first) return null;
  const n = group.activities.length;
  switch (first.kind) {
    case "tool_call": {
      const failed = group.activities.filter((a) => activityOutcome(a) === "failed").length;
      const open = group.activities.filter((a) => activityOutcome(a) === "open").length;
      if (n === 1) {
        const end = first.end;
        return (
          <>
            {first.tool}
            <span className="note-meta">
              {end
                ? end.outcome === "failed"
                  ? end.error.kind
                  : formatDuration(end.at - first.at)
                : "in flight"}
            </span>
          </>
        );
      }
      return (
        <>
          {first.tool} <span className="note-meta">×{n}</span>
          {failed > 0 && <span className="note-meta tone-critical">{failed} failed</span>}
          {open > 0 && <span className="note-meta">{open} in flight</span>}
        </>
      );
    }
    case "timer":
      return (
        <>
          sleep {formatDuration(first.durationMs)}
          {!first.end && (
            <span className="note-meta">fires {formatRelative(first.fireAt, now)}</span>
          )}
          {first.end?.outcome === "cancelled" && <span className="note-meta">cancelled</span>}
        </>
      );
    case "signal": {
      const end = first.end;
      return (
        <>
          signal "{first.name}"{n > 1 ? <span className="note-meta">×{n}</span> : null}
          <span className="note-meta">
            {end === null
              ? first.timeoutAt
                ? `gives up ${formatRelative(first.timeoutAt, now)}`
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

function Notes({
  model,
  line,
  selected,
  now,
  onSelectEntry,
  onSelectLine,
}: {
  model: RunModel;
  line: number;
  selected: number | null;
  now: number;
  onSelectEntry: (idx: number) => void;
  onSelectLine: (line: number) => void;
}) {
  const activities = model.byLine.get(line);
  if (!activities) return null;
  return (
    <span className="notes">
      {groupNotes(activities).map((g) => {
        const first = g.activities[0];
        if (!first) return null;
        const outcome = worst(g.activities);
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
              g.activities.length === 1 ? onSelectEntry(first.idx) : onSelectLine(line)
            }
          >
            {first.kind !== "log" && <OutcomeGlyph outcome={outcome} size={10} />}
            {noteText(g, now)}
          </button>
        );
      })}
    </span>
  );
}

export function RunView() {
  const { id = "" } = useParams();
  const [params, setParams] = useSearchParams();
  const run = useRun(id);
  const journal = useRunJournal(id);
  const script = useScript(run.data?.script);
  const model = useMemo(() => buildRunModel(journal.entries), [journal.entries]);
  const live = run.data ? !isEnded(run.data.status) && journal.state !== "ended" : false;
  const now = useNow(live ? 5_000 : 60_000);

  const selection = readSelection(params);
  const select = useCallback(
    (next: Selection) => {
      const p = new URLSearchParams(params);
      p.delete("entry");
      p.delete("line");
      if (next?.kind === "entry") p.set("entry", String(next.idx));
      if (next?.kind === "line") p.set("line", String(next.line));
      setParams(p, { replace: true });
    },
    [params, setParams],
  );
  const selectEntry = useCallback((idx: number) => select({ kind: "entry", idx }), [select]);
  const selectLine = useCallback((line: number) => select({ kind: "line", line }), [select]);

  const selectedIdx = selection?.kind === "entry" ? selection.idx : null;
  const selectedActivity = selectedIdx !== null ? model.byIdx.get(selectedIdx) : undefined;
  const selectedLine =
    selection?.kind === "line" ? selection.line : (selectedActivity?.callsite?.line ?? null);
  const errorLine = run.data?.error?.location?.line ?? null;

  const mark = useCallback(
    (line: number): LineMark | null => {
      if (line === errorLine) return "error";
      const activities = model.byLine.get(line);
      if (!activities) return null;
      const outcome = worst(activities);
      if (outcome === "failed") return "failed";
      if (outcome === "open") return "open";
      return "activity";
    },
    [model, errorLine],
  );

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
          <Link to="/runs">Back to the runs</Link>
        </p>
      </div>
    );
  }

  const r = run.data;
  const unattributed = model.unattributed.filter((a) => a.kind !== "timer" || a.timeoutOf === null);
  return (
    <div className="page run-page">
      <RunHeader run={r} model={model} state={journal.state} />
      {journal.state === "error" && (
        <ProblemView error={journal.error} title="The journal could not be read" />
      )}
      <RunBanners run={r} model={model} onSelectEntry={selectEntry} onSelectLine={selectLine} />

      <section className="panel" aria-label="Timeline">
        <header className="panel-head">
          <h2>Timeline</h2>
          {model.firstAt !== null && model.lastAt !== null && (
            <span className="muted num">
              {formatDuration((live ? now : model.lastAt) - model.firstAt)} from the first entry
              {live ? " to now" : " to the last"}
            </span>
          )}
        </header>
        {journal.entries.length === 0 && journal.state !== "ended" ? (
          <Loading>Reading the journal…</Loading>
        ) : (
          <Timeline
            model={model}
            now={live ? now : null}
            selected={selectedIdx}
            onSelect={selectEntry}
          />
        )}
      </section>

      <div className="run-body">
        <section className="panel source-panel" aria-label="Source">
          <header className="panel-head">
            <h2>
              <FileCode size={13} aria-hidden="true" style={{ verticalAlign: -2 }} /> script.ts
            </h2>
            <span className="muted">each line with what it caused</span>
            {unattributed.length > 0 && (
              <span className="end muted" title="Activities recorded without a callsite">
                {unattributed.length} without a line
              </span>
            )}
          </header>
          {script.isPending ? (
            <Loading>Loading the source…</Loading>
          ) : script.isError ? (
            <div className="panel-body">
              <ProblemView error={script.error} title="The source could not be read" />
            </div>
          ) : (
            <CodeView
              source={script.data.source}
              notesTitle="What each line caused"
              notes={(line) => (
                <Notes
                  model={model}
                  line={line}
                  selected={selectedIdx}
                  now={now}
                  onSelectEntry={selectEntry}
                  onSelectLine={selectLine}
                />
              )}
              mark={mark}
              selectedLine={selectedLine}
              focusLine={selectedLine}
              onLineClick={selectLine}
            />
          )}
        </section>
        <Inspector
          run={r}
          model={model}
          selection={selection}
          onSelectEntry={selectEntry}
          onSelectLine={selectLine}
          onClear={() => select(null)}
        />
      </div>

      <JournalPanel
        runId={r.id}
        model={model}
        selected={selectedIdx}
        onSelect={selectEntry}
        onSelectLine={selectLine}
      />
    </div>
  );
}
