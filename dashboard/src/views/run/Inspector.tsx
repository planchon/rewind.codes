// The details of what is selected: a tool call, a timer, a signal wait, a log, a journal entry or
// a source line; the run's input and output when nothing is.
import { X } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";
import { useRunEntry } from "../../api/queries";
import type { Entry, Run } from "../../api/types";
import { ProblemView, Time } from "../../components/bits";
import { JsonValue, PayloadView } from "../../components/Payload";
import { OutcomeBadge } from "../../components/Status";
import { formatDuration, formatOffset } from "../../model/format";
import type { Activity, RunModel } from "../../model/run";
import { activityOutcome, activityTitle, entrySummary } from "./describe";

export type Selection = { kind: "entry"; idx: number } | { kind: "line"; line: number } | null;

interface InspectorProps {
  run: Run;
  model: RunModel;
  selection: Selection;
  onSelectEntry: (idx: number) => void;
  onSelectLine: (line: number) => void;
  onClear: () => void;
}

function At({ ms, t0 }: { ms: number; t0: number | null }) {
  return (
    <>
      <Time ms={ms} withMs />{" "}
      {t0 !== null && <span className="muted num">{formatOffset(ms - t0)}</span>}
    </>
  );
}

function RawEntry({ runId, idx }: { runId: string; idx: number }) {
  const [open, setOpen] = useState(false);
  const entry = useRunEntry(runId, idx, open);
  return (
    <div className="raw-entry">
      <button
        type="button"
        className="link-btn mono"
        onClick={() => setOpen(!open)}
        aria-expanded={open}
      >
        entry #{idx} {open ? "▾" : "▸"}
      </button>
      {open &&
        (entry.isPending ? (
          <span className="spinner" />
        ) : entry.isError ? (
          <ProblemView error={entry.error} title="The entry could not be read" />
        ) : (
          <div className="payload-body">
            <JsonValue value={entry.data} />
          </div>
        ))}
    </div>
  );
}

function ActivityDetail({
  run,
  model,
  activity,
  onSelectEntry,
  onSelectLine,
}: {
  run: Run;
  model: RunModel;
  activity: Activity;
  onSelectEntry: (idx: number) => void;
  onSelectLine: (line: number) => void;
}) {
  const t0 = model.firstAt;
  const a = activity;
  const outcome = activityOutcome(a);
  const waiting =
    a.kind === "tool_call"
      ? run.waitingFor?.find((w) => w.kind === "tool_call" && w.seq === a.seq)
      : undefined;
  const line = a.callsite ? (
    <button
      type="button"
      className="link-btn mono"
      onClick={() => a.callsite && onSelectLine(a.callsite.line)}
    >
      script.ts:{a.callsite.line}:{a.callsite.column}
    </button>
  ) : (
    <span className="muted">no callsite</span>
  );

  return (
    <div className="inspector-body">
      <div className="inspector-title">
        <h3 className="mono">{activityTitle(a)}</h3>
        {a.kind !== "log" && <OutcomeBadge outcome={outcome} />}
      </div>
      <dl className="kv">
        <dt>line</dt>
        <dd>{line}</dd>
        {a.kind === "tool_call" && (
          <>
            <dt>tool</dt>
            <dd>
              <Link
                to={`/runs?tool=${encodeURIComponent(a.tool)}`}
                className="mono"
                title="Runs that called this tool"
              >
                {a.tool}
              </Link>
            </dd>
          </>
        )}
        <dt>
          {a.kind === "tool_call"
            ? "scheduled"
            : a.kind === "signal"
              ? "awaited"
              : a.kind === "timer"
                ? "started"
                : "at"}
        </dt>
        <dd>
          <At ms={a.at} t0={t0} />
        </dd>
        {a.kind === "timer" && (
          <>
            <dt>duration</dt>
            <dd className="num">{formatDuration(a.durationMs)}</dd>
            <dt>fires at</dt>
            <dd>
              <At ms={a.fireAt} t0={t0} />
            </dd>
          </>
        )}
        {a.kind === "signal" && a.timeoutAt !== null && (
          <>
            <dt>gives up at</dt>
            <dd>
              <At ms={a.timeoutAt} t0={t0} />
            </dd>
          </>
        )}
        {a.kind !== "log" && a.end && (
          <>
            <dt>
              {a.end.outcome === "failed"
                ? "failed"
                : a.end.outcome === "cancelled"
                  ? "cancelled"
                  : a.end.outcome === "timed_out"
                    ? "timed out"
                    : a.kind === "signal"
                      ? "received"
                      : a.kind === "timer"
                        ? "fired"
                        : "completed"}
            </dt>
            <dd>
              <At ms={a.end.at} t0={t0} />
            </dd>
            <dt>{a.kind === "signal" ? "waited" : "took"}</dt>
            <dd className="num">{formatDuration(a.end.at - a.at)}</dd>
          </>
        )}
        {waiting?.kind === "tool_call" && (
          <>
            <dt>task</dt>
            <dd>
              <span className="mono">{waiting.state}</span>
              {waiting.parked && <span className="mono"> · {waiting.parked}</span>}
              {waiting.attempt > 1 && <span> · attempt {waiting.attempt}</span>}
              <span className="muted mono"> · {waiting.taskId}</span>
            </dd>
          </>
        )}
        {a.kind === "timer" && a.timeoutOf !== null && (
          <>
            <dt>timeout of</dt>
            <dd>
              <button
                type="button"
                className="link-btn"
                onClick={() => a.timeoutOf !== null && onSelectEntry(a.timeoutOf)}
              >
                the signal wait at entry #{a.timeoutOf}
              </button>
            </dd>
          </>
        )}
        {a.kind === "signal" && a.end?.outcome === "received" && a.end.buffered && (
          <>
            <dt>mailbox</dt>
            <dd>The signal arrived before the script waited for it.</dd>
          </>
        )}
        <dt>entries</dt>
        <dd className="entry-links">
          <RawEntry runId={run.id} idx={a.idx} />
          {a.kind !== "log" && a.end && <RawEntry runId={run.id} idx={a.end.idx} />}
        </dd>
      </dl>
      {a.kind === "log" && <pre className="log-message">{a.message}</pre>}
      {a.kind === "tool_call" && <PayloadView runId={run.id} name="args" payload={a.args} />}
      {a.kind === "tool_call" && a.end?.outcome === "ok" && (
        <PayloadView runId={run.id} name="result" payload={a.end.result} />
      )}
      {a.kind === "tool_call" && a.end?.outcome === "failed" && (
        <section className="failure">
          <h4>
            <span className="mono">{a.end.error.kind}</span>: {a.end.error.message}
          </h4>
          {a.end.error.data !== undefined && (
            <div className="payload-body">
              <JsonValue value={a.end.error.data} />
            </div>
          )}
        </section>
      )}
      {a.kind === "signal" && a.end?.outcome === "received" && (
        <PayloadView runId={run.id} name="payload" payload={a.end.payload} />
      )}
    </div>
  );
}

function EntryDetail({ run, model, entry }: { run: Run; model: RunModel; entry: Entry }) {
  const e = entry.event;
  return (
    <div className="inspector-body">
      <div className="inspector-title">
        <h3 className="mono">{e.type}</h3>
        <span className="muted mono">#{entry.idx}</span>
      </div>
      <dl className="kv">
        <dt>at</dt>
        <dd>
          <At ms={Date.parse(entry.recordedAt)} t0={model.firstAt} />
        </dd>
        <dt>event</dt>
        <dd>{entrySummary(entry)}</dd>
        <dt>entry</dt>
        <dd>
          <RawEntry runId={run.id} idx={entry.idx} />
        </dd>
      </dl>
      {e.type === "run_started" && <PayloadView runId={run.id} name="input" payload={e.input} />}
      {e.type === "complete_run" && <PayloadView runId={run.id} name="output" payload={e.output} />}
      {e.type === "signal_received" && (
        <PayloadView runId={run.id} name="payload" payload={e.payload} />
      )}
      {e.type === "fail_run" && e.error.stack && <pre className="frame">{e.error.stack}</pre>}
    </div>
  );
}

function LineDetail({
  model,
  line,
  onSelectEntry,
}: {
  model: RunModel;
  line: number;
  onSelectEntry: (idx: number) => void;
}) {
  const activities = model.byLine.get(line) ?? [];
  return (
    <div className="inspector-body">
      <div className="inspector-title">
        <h3 className="mono">script.ts:{line}</h3>
        <span className="muted">
          {activities.length === 0 ? "nothing recorded" : `${activities.length} recorded`}
        </span>
      </div>
      {activities.length > 0 && (
        <ul className="line-list">
          {activities.map((a) => (
            <li key={a.idx}>
              <button type="button" className="line-item" onClick={() => onSelectEntry(a.idx)}>
                {a.kind === "log" ? (
                  <span className={`log-level level-${a.level}`}>{a.level}</span>
                ) : (
                  <OutcomeBadge outcome={activityOutcome(a)} word="" />
                )}
                <span className="mono">{a.kind === "log" ? a.message : activityTitle(a)}</span>
                {a.kind !== "log" && a.end && (
                  <span className="muted num">{formatDuration(a.end.at - a.at)}</span>
                )}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Overview({ run }: { run: Run }) {
  return (
    <div className="inspector-body">
      <p className="muted" style={{ margin: 0 }}>
        Select a line, a note beside it, a span of the timeline or a journal entry.
      </p>
      {run.input && <PayloadView runId={run.id} name="input" payload={run.input} />}
      {run.output && <PayloadView runId={run.id} name="output" payload={run.output} />}
    </div>
  );
}

export function Inspector({
  run,
  model,
  selection,
  onSelectEntry,
  onSelectLine,
  onClear,
}: InspectorProps) {
  let body = <Overview run={run} />;
  let title = "Run";
  if (selection?.kind === "line") {
    title = "Line";
    body = <LineDetail model={model} line={selection.line} onSelectEntry={onSelectEntry} />;
  } else if (selection?.kind === "entry") {
    const activity = model.byIdx.get(selection.idx);
    const entry = model.entries[selection.idx];
    if (activity) {
      title =
        activity.kind === "tool_call"
          ? "Tool call"
          : activity.kind === "timer"
            ? "Timer"
            : activity.kind === "signal"
              ? "Signal wait"
              : "Log";
      body = (
        <ActivityDetail
          run={run}
          model={model}
          activity={activity}
          onSelectEntry={onSelectEntry}
          onSelectLine={onSelectLine}
        />
      );
    } else if (entry) {
      title = "Entry";
      body = <EntryDetail run={run} model={model} entry={entry} />;
    } else {
      body = <p className="muted inspector-body">Entry #{selection.idx} has not arrived yet.</p>;
    }
  }
  return (
    <aside className="panel inspector" aria-label="Inspector">
      <header className="panel-head">
        <h2>{title}</h2>
        {selection && (
          <span className="end">
            <button
              type="button"
              className="icon-btn"
              aria-label="Clear the selection"
              onClick={onClear}
            >
              <X size={14} />
            </button>
          </span>
        )}
      </header>
      {body}
    </aside>
  );
}
