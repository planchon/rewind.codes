// The details of what is selected: a tool call, a timer, a signal wait, a log, a journal entry or
// a source line; the run's input and output when nothing is.
import { X } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";
import { useRunEntry } from "../../api/queries";
import type { Entry, Payload, Run } from "../../api/types";
import { ProblemView, Time } from "../../components/bits";
import { JsonBox, PayloadView } from "../../components/Payload";
import { OutcomeBadge, StatusBadge } from "../../components/Status";
import { formatDuration, formatOffset } from "../../model/format";
import { fieldTerm, queryLink } from "../../model/query";
import type { Activity, RunModel } from "../../model/run";
import { type Step, stepOf } from "../../model/steps";
import {
  endedRunOf,
  readRunEnd,
  signalTargetOf,
  startedRunOf,
  workflowCallOf,
} from "../../model/workflows";
import { activityOutcome, activityTitle, entrySummary } from "./describe";

export type Selection = { kind: "entry"; idx: number } | { kind: "line"; line: number } | null;

interface InspectorProps {
  run: Run;
  model: RunModel;
  selection: Selection;
  onSelectEntry: (idx: number) => void;
  onSelectLine: (line: number) => void;
  onClear: () => void;
  /** The steps of the time cursor, to move it to an activity's start or end. */
  steps?: readonly Step[];
  onGoStep?: (n: number) => void;
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
            <JsonBox name={`entry #${idx}`} value={entry.data} />
          </div>
        ))}
    </div>
  );
}

/** The steps an activity started and ended in, each a button that moves the cursor there. */
function StepLinks({
  activity: a,
  steps,
  onGoStep,
}: {
  activity: Activity;
  steps: readonly Step[];
  onGoStep: (n: number) => void;
}) {
  const start = stepOf(steps, a.idx);
  const end = a.kind !== "log" && a.end ? stepOf(steps, a.end.idx) : -1;
  if (start < 0) return null;
  return (
    <>
      <dt>steps</dt>
      <dd>
        <button type="button" className="link-btn" onClick={() => onGoStep(start)}>
          started in step {start + 1}
        </button>
        {end >= 0 && (
          <>
            {", "}
            <button type="button" className="link-btn" onClick={() => onGoStep(end)}>
              ended in step {end + 1}
            </button>
          </>
        )}
      </dd>
    </>
  );
}

/** A link to another run, by its full id. */
function RunLink({ id }: { id: string }) {
  return (
    <Link to={`/runs/${id}`} className="mono" title="Open this run">
      {id}
    </Link>
  );
}

/** How a run this run started ended, from the payload of its `rewind:ended:` signal. */
function RunEndRows({ runId, payload }: { runId: string; payload: Payload }) {
  const end = readRunEnd(payload);
  return (
    <>
      <dt>run</dt>
      <dd>
        <RunLink id={runId} />
      </dd>
      <dt>it ended</dt>
      <dd>
        {end.status ? <StatusBadge status={end.status} /> : <span className="muted">ended</span>}
        {end.message && <span className="muted"> {end.message}</span>}
      </dd>
    </>
  );
}

function ActivityDetail({
  run,
  model,
  activity,
  onSelectEntry,
  onSelectLine,
  steps,
  onGoStep,
}: {
  run: Run;
  model: RunModel;
  activity: Activity;
  onSelectEntry: (idx: number) => void;
  onSelectLine: (line: number) => void;
  steps?: readonly Step[] | undefined;
  onGoStep?: ((n: number) => void) | undefined;
}) {
  const t0 = model.firstAt;
  const a = activity;
  const outcome = activityOutcome(a, model.end !== null);
  const workflowCall = a.kind === "tool_call" ? workflowCallOf(a.tool) : null;
  const startedRun = a.kind === "tool_call" ? startedRunOf(a) : null;
  const signalTarget =
    a.kind === "tool_call" && workflowCall?.op === "signal" ? signalTargetOf(a.args) : null;
  const endedRun = a.kind === "signal" ? endedRunOf(a.name) : null;
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
                to={queryLink("/observability", fieldTerm("tool", a.tool))}
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
        {startedRun !== null && (
          <>
            <dt>started</dt>
            <dd>
              <RunLink id={startedRun} />
            </dd>
          </>
        )}
        {signalTarget !== null && (
          <>
            <dt>to run</dt>
            <dd>
              <RunLink id={signalTarget} />
            </dd>
          </>
        )}
        {endedRun !== null && a.kind === "signal" && a.end?.outcome === "received" && (
          <RunEndRows runId={endedRun} payload={a.end.payload} />
        )}
        {endedRun !== null && a.kind === "signal" && a.end === null && (
          <>
            <dt>run</dt>
            <dd>
              <RunLink id={endedRun} />
            </dd>
          </>
        )}
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
        {steps && onGoStep && <StepLinks activity={a} steps={steps} onGoStep={onGoStep} />}
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
              <JsonBox name="error data" value={a.end.error.data} />
            </div>
          )}
        </section>
      )}
      {a.kind === "signal" && a.end?.outcome === "received" && (
        <PayloadView
          runId={run.id}
          name={endedRun !== null ? "end" : "payload"}
          payload={a.end.payload}
        />
      )}
    </div>
  );
}

function EntryDetail({ run, model, entry }: { run: Run; model: RunModel; entry: Entry }) {
  const e = entry.event;
  const endedRun = e.type === "signal_received" ? endedRunOf(e.name) : null;
  return (
    <div className="inspector-body">
      <div className="inspector-title">
        <h3 className="mono">{endedRun !== null ? "end of a run it started" : e.type}</h3>
        <span className="muted mono">#{entry.idx}</span>
      </div>
      <dl className="kv">
        <dt>at</dt>
        <dd>
          <At ms={Date.parse(entry.recordedAt)} t0={model.firstAt} />
        </dd>
        <dt>event</dt>
        <dd>{entrySummary(entry)}</dd>
        {endedRun !== null && e.type === "signal_received" && (
          <RunEndRows runId={endedRun} payload={e.payload} />
        )}
        <dt>entry</dt>
        <dd>
          <RawEntry runId={run.id} idx={entry.idx} />
        </dd>
      </dl>
      {e.type === "run_started" && <PayloadView runId={run.id} name="input" payload={e.input} />}
      {e.type === "complete_run" && <PayloadView runId={run.id} name="output" payload={e.output} />}
      {e.type === "signal_received" && (
        <PayloadView
          runId={run.id}
          name={endedRun !== null ? "end" : "payload"}
          payload={e.payload}
        />
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
                  <OutcomeBadge outcome={activityOutcome(a, model.end !== null)} word="" />
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
  steps,
  onGoStep,
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
              ? endedRunOf(activity.name) !== null
                ? "Result of a run"
                : "Signal wait"
              : "Log";
      body = (
        <ActivityDetail
          run={run}
          model={model}
          activity={activity}
          onSelectEntry={onSelectEntry}
          onSelectLine={onSelectLine}
          steps={steps}
          onGoStep={onGoStep}
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
