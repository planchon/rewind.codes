// The time cursor of the Run page (archi/05-testing-and-observability.md, "Dashboard"): the
// steps of the history, which one the page shows (`?step=`, the head when absent), the bar that
// moves it, the panel of what the step did, the values a hover shows and the watches. Only values
// that crossed the journal are known: what the script computes in between is not recorded.
import { ChevronFirst, ChevronLast, ChevronLeft, ChevronRight, X } from "lucide-react";
import { type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router";
import { usePayloadValue } from "../../api/queries";
import type { Entry, Payload } from "../../api/types";
import { useSettings } from "../../app/settings";
import { ProblemView } from "../../components/bits";
import { JsonBox } from "../../components/Payload";
import { EndGlyph } from "../../components/Status";
import { type EndKind, pendingBefore } from "../../model/ending";
import { formatBytes, formatDuration, formatOffset, formatTime } from "../../model/format";
import type { Activity, RunModel } from "../../model/run";
import {
  buildSteps,
  issuedIn,
  isTimeout,
  markSymbol,
  moveStep,
  type Step,
  stateAt,
  stepFromParam,
  stepOf,
} from "../../model/steps";
import { ENDED } from "./RunEnd";
import type { BindingTarget, Target } from "./targets";

export interface TimeCursor {
  steps: Step[];
  /** The step shown, from 0; -1 before the first entry. */
  n: number;
  /** The cursor follows the head: no `?step=`. */
  atHead: boolean;
  step: Step | null;
  /** The last entry the cursor includes. */
  cursor: number;
  every: boolean;
  setEvery: (every: boolean) => void;
  go: (n: number) => void;
  move: (by: 1 | -1) => void;
}

export function useTimeCursor(entries: readonly Entry[]): TimeCursor {
  const [params, setParams] = useSearchParams();
  const steps = useMemo(() => buildSteps(entries), [entries]);
  const head = steps.length - 1;
  const pinned = stepFromParam(steps, params.get("step"));
  const n = pinned ?? head;
  const [every, setEvery] = useState(false);
  const go = useCallback(
    (to: number) => {
      const target = Math.max(0, Math.min(head, to));
      setParams(
        (current) => {
          const next = new URLSearchParams(current);
          const step = steps[target];
          if (target >= head || !step) next.delete("step");
          else next.set("step", String(step.first));
          return next;
        },
        { replace: true },
      );
    },
    [head, steps, setParams],
  );
  const move = useCallback(
    (by: 1 | -1) => go(moveStep(steps, n, by, every)),
    [go, steps, n, every],
  );
  const step = steps[n] ?? null;
  return {
    steps,
    n,
    atHead: pinned === null,
    step,
    cursor: step?.last ?? entries.length - 1,
    every,
    setEvery,
    go,
    move,
  };
}

/** ←, →, Home and End move the cursor, unless the operator is typing. */
export function useCursorKeys(cursor: TimeCursor) {
  const ref = useRef(cursor);
  ref.current = cursor;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return;
      const target = e.target as HTMLElement | null;
      if (target?.closest?.("textarea, select, [contenteditable='true']")) return;
      if (
        target instanceof HTMLInputElement &&
        !["checkbox", "radio", "button"].includes(target.type)
      )
        return;
      const c = ref.current;
      if (c.steps.length === 0) return;
      if (e.key === "ArrowRight") c.move(1);
      else if (e.key === "ArrowLeft") c.move(-1);
      else if (e.key === "Home") c.go(0);
      else if (e.key === "End") c.go(c.steps.length - 1);
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
}

export function CursorBar({
  cursor,
  live,
  ending,
  t0,
}: {
  cursor: TimeCursor;
  live: boolean;
  /** The step the run ended in, and how. */
  ending: { step: number; kind: EndKind } | null;
  t0: number | null;
}) {
  const { zone } = useSettings();
  const { steps, n, step, atHead } = cursor;
  if (!step) return null;
  const last = steps.length - 1;
  return (
    <section className="cursor-bar" aria-label="Time cursor">
      <div className="cursor-pos" aria-live="polite">
        <span className="cursor-step">
          Step {n + 1} of {steps.length}
        </span>
        <span className="cursor-when num">
          {formatTime(step.at, zone)}
          {zone === "utc" ? " UTC" : ""}
          {t0 !== null ? ` · ${formatOffset(step.at - t0)}` : ""} · entries {step.first}
          {step.last > step.first ? `–${step.last}` : ""}
        </span>
      </div>
      {/* Right-aligned, each control of a fixed width: moving the cursor moves none of them. */}
      <div className="cursor-controls">
        {live &&
          (atHead ? (
            <span className="cursor-live tone-info">following the head</span>
          ) : (
            <button type="button" className="link-btn cursor-live" onClick={() => cursor.go(last)}>
              Follow the head
            </button>
          ))}
        {ending && (
          <button
            type="button"
            className={`end-chip end-chip-${ending.kind}`}
            aria-current={ending.step === n ? "step" : undefined}
            title={ending.step === n ? "The step shown" : `Go to step ${ending.step + 1}`}
            onClick={() => cursor.go(ending.step)}
          >
            <EndGlyph kind={ending.kind} size={12} />
            {ENDED[ending.kind]} in step {ending.step + 1}
          </button>
        )}
        <label className="check" title="Also stop at the steps that issue nothing">
          <input
            type="checkbox"
            checked={cursor.every}
            onChange={(e) => cursor.setEvery(e.target.checked)}
          />
          every step
        </label>
        <div className="cursor-nav">
          <button
            type="button"
            className="icon-btn"
            aria-label="First step"
            title="First step (Home)"
            disabled={n === 0}
            onClick={() => cursor.go(0)}
          >
            <ChevronFirst size={15} />
          </button>
          <button
            type="button"
            className="btn small"
            disabled={n === 0}
            onClick={() => cursor.move(-1)}
            title="The previous step (←)"
          >
            <ChevronLeft size={13} /> Step back
          </button>
          <button
            type="button"
            className="btn small"
            disabled={n === last}
            onClick={() => cursor.move(1)}
            title="The next step (→)"
          >
            Step <ChevronRight size={13} />
          </button>
          <button
            type="button"
            className="icon-btn"
            aria-label="Last step"
            title="Last step (End)"
            disabled={n === last}
            onClick={() => cursor.go(last)}
          >
            <ChevronLast size={15} />
          </button>
        </div>
      </div>
    </section>
  );
}

function applyPath(value: unknown, path: string | number | null): unknown {
  if (path === null) return value;
  if (value === null || typeof value !== "object") return undefined;
  return (value as Record<string | number, unknown>)[path];
}

function short(value: unknown, max = 90): string {
  const text = JSON.stringify(value) ?? "undefined";
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

function LoadedValue({
  payload,
  path,
  runId,
  compact,
}: {
  payload: Payload;
  path: string | number | null;
  runId: string;
  compact: boolean;
}) {
  const value = usePayloadValue(payload.digest, runId, true);
  if (value.isPending) return <span className="spinner" />;
  if (value.isError)
    return <ProblemView error={value.error} title="The value could not be loaded" />;
  const v = applyPath(value.data, path);
  return compact ? <code className="value-short">{short(v)}</code> : <JsonBlock value={v} />;
}

function JsonBlock({ value }: { value: unknown }) {
  return (
    <div className="payload-body value-block">
      <JsonBox name="value" value={value} />
    </div>
  );
}

/** A payload's value, or its field: inline, or read on demand when the entry left it out. */
export function PayloadValue({
  payload,
  path = null,
  runId,
  compact = false,
}: {
  payload: Payload;
  path?: string | number | null;
  runId: string;
  compact?: boolean;
}) {
  const [asked, setAsked] = useState(false);
  if (payload.value !== undefined) {
    const v = applyPath(payload.value, path);
    return compact ? <code className="value-short">{short(v)}</code> : <JsonBlock value={v} />;
  }
  if (asked) return <LoadedValue payload={payload} path={path} runId={runId} compact={compact} />;
  return (
    <span className="value-load">
      {payload.preview && path === null && (
        <code className="value-short">{short(payload.preview, 60)}</code>
      )}
      <button
        type="button"
        className="btn small"
        onClick={(e) => {
          e.stopPropagation();
          setAsked(true);
        }}
      >
        Load the value ({formatBytes(payload.bytes)})
      </button>
    </span>
  );
}

/** "step 4", from an entry. */
function stepWord(steps: readonly Step[], idx: number): string {
  return `step ${stepOf(steps, idx) + 1}`;
}

function StatePill({ a, cursor }: { a: Activity; cursor: number }) {
  const state = stateAt(a, cursor);
  let word: string;
  let tone: string;
  if (state === "future") {
    word = "not yet";
    tone = "future";
  } else if (state === "open") {
    word = a.kind === "tool_call" ? "in flight" : a.kind === "timer" ? "pending" : "waiting";
    tone = "open";
  } else if (a.kind === "log") {
    word = "done";
    tone = "done";
  } else {
    const outcome = a.end?.outcome ?? "ok";
    word = {
      ok: "returned",
      failed: "failed",
      fired: "fired",
      cancelled: "cancelled",
      received: "received",
      timed_out: "timed out",
    }[outcome];
    tone = outcome === "failed" ? "failed" : "done";
  }
  return <span className={`pill pill-${tone}`}>{word}</span>;
}

function activityName(a: Activity): string {
  switch (a.kind) {
    case "tool_call":
      return `#${a.seq} ${a.tool}`;
    case "timer":
      return `sleep ${formatDuration(a.durationMs)} · timer #${a.seq}`;
    case "signal":
      return `signal "${a.name}"`;
    case "log":
      return `console.${a.level}`;
  }
}

/** One activity as the cursor knows it: what went in, what came out, or when it will. */
function ActivityAt({
  a,
  cursor,
  steps,
  runId,
  zone,
}: {
  a: Activity;
  cursor: number;
  steps: readonly Step[];
  runId: string;
  zone: "utc" | "local";
}) {
  const state = stateAt(a, cursor);
  const time = (ms: number) => `${formatTime(ms, zone)}${zone === "utc" ? " UTC" : ""}`;
  const where =
    a.kind === "log"
      ? stepWord(steps, a.idx)
      : `started in ${stepWord(steps, a.idx)}${a.end ? `, ended in ${stepWord(steps, a.end.idx)}` : ""}`;
  let input: ReactNode = null;
  let output: ReactNode;
  switch (a.kind) {
    case "tool_call":
      input = <PayloadValue payload={a.args} runId={runId} />;
      if (state === "future")
        output = <p className="meta">Not called yet: it is called in {stepWord(steps, a.idx)}.</p>;
      else if (state === "open")
        output = <p className="meta">In flight since {stepWord(steps, a.idx)}.</p>;
      else if (a.end?.outcome === "ok")
        output = <PayloadValue payload={a.end.result} runId={runId} />;
      else if (a.end?.outcome === "failed")
        output = (
          <p className="meta tone-critical">
            <span className="mono">{a.end.error.kind}</span>: {a.end.error.message}
          </p>
        );
      break;
    case "timer":
      input = (
        <p className="meta">
          {formatDuration(a.durationMs)}, due {time(a.fireAt)}
        </p>
      );
      if (state === "future")
        output = <p className="meta">Not started yet: it is in {stepWord(steps, a.idx)}.</p>;
      else if (state === "open") output = <p className="meta">Due {time(a.fireAt)}.</p>;
      else
        output = (
          <p className="meta">
            {a.end?.outcome === "fired" ? "Fired" : "Cancelled"} in{" "}
            {stepWord(steps, a.end?.idx ?? a.idx)}.
          </p>
        );
      break;
    case "signal":
      input = (
        <p className="meta">
          {a.timeoutAt !== null ? `timeout: gives up ${time(a.timeoutAt)}` : "no timeout"}
        </p>
      );
      if (state === "future")
        output = <p className="meta">Not awaited yet: it is in {stepWord(steps, a.idx)}.</p>;
      else if (state === "open")
        output = (
          <p className="meta">
            Waiting since {stepWord(steps, a.idx)}
            {a.timeoutAt !== null ? `; gives up ${time(a.timeoutAt)} and returns undefined` : ""}.
          </p>
        );
      else if (a.end?.outcome === "received")
        output = <PayloadValue payload={a.end.payload} runId={runId} />;
      else output = <p className="meta">Timed out: it returned undefined.</p>;
      break;
    case "log":
      output = <p className="meta">{a.message}</p>;
      break;
  }
  return (
    <div className="value-activity">
      <div className="value-head">
        <span className="mono">{activityName(a)}</span>
        <StatePill a={a} cursor={cursor} />
      </div>
      {input !== null && (
        <>
          <div className="value-label">Input</div>
          {input}
        </>
      )}
      <div className="value-label">Output</div>
      {output}
      <p className="meta muted">{where}</p>
    </div>
  );
}

/** What a binding holds at the cursor: the latest awaited result of its call, or why not yet. */
function bindingAt(target: BindingTarget, cursor: number) {
  const acts = target.call.activities;
  const done = acts.filter((a) => stateAt(a, cursor) === "done");
  const latest = done[done.length - 1];
  const open = acts.find((a) => stateAt(a, cursor) === "open");
  return { acts, latest, open, iteration: latest ? acts.indexOf(latest) + 1 : 0 };
}

function BindingValue({
  target,
  cursor,
  steps,
  runId,
  compact,
}: {
  target: BindingTarget;
  cursor: number;
  steps: readonly Step[];
  runId: string;
  compact: boolean;
}) {
  const { acts, latest, open, iteration } = bindingAt(target, cursor);
  if (!latest || (open && open.idx > latest.idx)) {
    const first = open ?? acts[0];
    const why = open
      ? `the await is still open, since ${stepWord(steps, open.idx)}`
      : first && first.kind !== "log" && first.end
        ? `it is assigned in ${stepWord(steps, first.end.idx)}`
        : "its call has not ended";
    return <span className="meta">not assigned yet: {why}</span>;
  }
  const field = target.path === null ? null : target.path;
  let value: ReactNode;
  if (latest.kind === "tool_call" && latest.end?.outcome === "ok")
    value = (
      <PayloadValue payload={latest.end.result} path={field} runId={runId} compact={compact} />
    );
  else if (latest.kind === "tool_call" && latest.end?.outcome === "failed")
    value = (
      <span className="meta tone-critical">
        the await threw {latest.end.error.kind}: {latest.end.error.message}
      </span>
    );
  else if (latest.kind === "signal" && latest.end?.outcome === "received")
    value = (
      <PayloadValue payload={latest.end.payload} path={field} runId={runId} compact={compact} />
    );
  else value = <code className="value-short">undefined</code>;
  return (
    <>
      {value}
      {acts.length > 1 && (
        <span className="meta muted">
          {" "}
          iteration {iteration} of {acts.length}
        </span>
      )}
    </>
  );
}

/** The source of a binding, as the popover says it. */
function bindingSource(target: BindingTarget): string {
  const a = target.call.activities[0];
  const of = !a
    ? "its call"
    : a.kind === "tool_call"
      ? `#${a.seq} ${a.tool}`
      : a.kind === "signal"
        ? `signal "${a.name}"`
        : "the call";
  const what = a?.kind === "signal" ? "payload" : "result";
  if (target.path === null) return `= the ${what} of ${of}`;
  return `= the ${what} of ${of}, ${typeof target.path === "number" ? `[${target.path}]` : `.${target.path}`}`;
}

export interface ValueContext {
  model: RunModel;
  steps: readonly Step[];
  cursor: number;
  runId: string;
  input: Payload | null;
}

/** The popover of a target at the cursor. */
export function TargetDetail({ target, ctx }: { target: Target; ctx: ValueContext }) {
  const { zone } = useSettings();
  if (target.kind === "input") {
    return (
      <>
        <div className="value-head">
          <span className="mono">{target.name}</span>
        </div>
        <p className="meta">
          The run's input{target.path !== null ? `, .${target.path}` : ""}, from run_started.
        </p>
        {ctx.input ? (
          <PayloadValue payload={ctx.input} path={target.path} runId={ctx.runId} />
        ) : (
          <p className="meta">The run has not started.</p>
        )}
      </>
    );
  }
  if (target.kind === "binding") {
    return (
      <>
        <div className="value-head">
          <span className="mono">{target.name}</span>
        </div>
        <p className="meta">{bindingSource(target)}</p>
        <BindingValue
          target={target}
          cursor={ctx.cursor}
          steps={ctx.steps}
          runId={ctx.runId}
          compact={false}
        />
      </>
    );
  }
  const acts = target.activities;
  const shown = acts.slice(0, 8);
  return (
    <>
      {acts.length > 1 && (
        <p className="meta">
          {acts.length} {acts[0]?.kind === "tool_call" ? "calls" : "activities"} from this line (a
          loop or a fan-out):
        </p>
      )}
      {shown.map((a) => (
        <ActivityAt
          key={a.idx}
          a={a}
          cursor={ctx.cursor}
          steps={ctx.steps}
          runId={ctx.runId}
          zone={zone}
        />
      ))}
      {acts.length > shown.length && (
        <p className="meta muted">and {acts.length - shown.length} more</p>
      )}
    </>
  );
}

/** A watch's name: the variable, or the call's line and callee. */
export function targetName(target: Target): string {
  if (target.kind !== "call") return target.name;
  const text = target.text.replace(/\s+/g, " ");
  const callee = text.slice(0, text.indexOf("(") >= 0 ? text.indexOf("(") : undefined);
  return `L${target.span.line} ${callee}(…)`;
}

/** A watch's value at the cursor, on one line. */
function WatchValue({ target, ctx }: { target: Target; ctx: ValueContext }) {
  if (target.kind === "input")
    return ctx.input ? (
      <PayloadValue payload={ctx.input} path={target.path} runId={ctx.runId} compact />
    ) : (
      <span className="meta">not started</span>
    );
  if (target.kind === "binding")
    return (
      <BindingValue
        target={target}
        cursor={ctx.cursor}
        steps={ctx.steps}
        runId={ctx.runId}
        compact
      />
    );
  return (
    <span className="watch-calls">
      {target.activities.slice(0, 4).map((a) => (
        <span key={a.idx} className="watch-call">
          <span className="muted mono">{a.kind === "tool_call" ? `#${a.seq}` : a.kind}</span>{" "}
          <CallOutcome a={a} ctx={ctx} />
        </span>
      ))}
      {target.activities.length > 4 && (
        <span className="muted">+{target.activities.length - 4}</span>
      )}
    </span>
  );
}

function CallOutcome({ a, ctx }: { a: Activity; ctx: ValueContext }) {
  const state = stateAt(a, ctx.cursor);
  if (state === "future") return <span className="meta">not yet</span>;
  if (state === "open")
    return <span className="meta">{a.kind === "tool_call" ? "in flight" : "waiting"}</span>;
  if (a.kind === "tool_call" && a.end?.outcome === "ok")
    return <PayloadValue payload={a.end.result} runId={ctx.runId} compact />;
  if (a.kind === "tool_call" && a.end?.outcome === "failed")
    return <span className="meta tone-critical">{a.end.error.kind}</span>;
  if (a.kind === "signal" && a.end?.outcome === "received")
    return <PayloadValue payload={a.end.payload} runId={ctx.runId} compact />;
  return (
    <span className="meta">
      {a.kind !== "log" ? (a.end?.outcome ?? "done").replace("_", " ") : "done"}
    </span>
  );
}

/** The popover of a hovered target, kept open while the pointer is over it. */
export function ValuePopover({
  target,
  anchor,
  ctx,
  pinned,
  onEnter,
  onLeave,
}: {
  target: Target;
  anchor: DOMRect;
  ctx: ValueContext;
  pinned: boolean;
  onEnter: () => void;
  onLeave: () => void;
}) {
  const width = 420;
  const left = Math.max(8, Math.min(anchor.left, window.innerWidth - width - 8));
  const below = anchor.bottom + 6;
  const top = below + 280 > window.innerHeight && anchor.top > 300 ? undefined : below;
  const bottom = top === undefined ? window.innerHeight - anchor.top + 6 : undefined;
  return (
    <div
      className="value-pop"
      role="tooltip"
      style={{ left, top, bottom, width }}
      onMouseEnter={onEnter}
      onMouseLeave={onLeave}
    >
      <TargetDetail target={target} ctx={ctx} />
      <p className="meta muted value-foot">
        {pinned ? "Watched: click to stop watching" : "Click to watch it as the cursor moves"}
      </p>
    </div>
  );
}

/** "Woke up because", "Then it", "Now paused on", and the watches. */
export function StepPanel({
  cursor,
  model,
  watches,
  targets,
  ctx,
  onUnwatch,
  onSelectLine,
  onSelectEntry,
}: {
  cursor: TimeCursor;
  model: RunModel;
  watches: string[];
  targets: Map<string, Target>;
  ctx: ValueContext;
  onUnwatch: (key: string) => void;
  onSelectLine: (line: number) => void;
  onSelectEntry: (idx: number) => void;
}) {
  const { zone } = useSettings();
  const step = cursor.step;
  if (!step) return null;
  const time = (ms: number) => `${formatTime(ms, zone)}${zone === "utc" ? " UTC" : ""}`;
  const issued = issuedIn(model, step);
  const rank = (idx: number) => {
    const a = model.byIdx.get(idx);
    const at = a ? issued.indexOf(a) : -1;
    return at >= 0 ? markSymbol({ kind: "issued", rank: at + 1 }) : "·";
  };
  const line = (n: number | undefined) =>
    n === undefined ? null : (
      <button type="button" className="link-btn mono ln" onClick={() => onSelectLine(n)}>
        L{n}
      </button>
    );

  const e = step.trigger.event;
  let woke: ReactNode;
  switch (e.type) {
    case "run_started":
      woke = (
        <>
          The run started with <PayloadValue payload={e.input} runId={ctx.runId} compact />
        </>
      );
      break;
    case "tool_call_completed": {
      const a = model.byIdx.get(step.first);
      woke = (
        <>
          <b className="mono">
            #{e.seq} {a?.kind === "tool_call" ? a.tool : ""}
          </b>{" "}
          returned <PayloadValue payload={e.result} runId={ctx.runId} compact />
        </>
      );
      break;
    }
    case "tool_call_failed": {
      const a = model.byIdx.get(step.first);
      woke = (
        <span className="tone-critical">
          <b className="mono">
            #{e.seq} {a?.kind === "tool_call" ? a.tool : ""}
          </b>{" "}
          failed: <span className="mono">{e.error.kind}</span>, {e.error.message}
        </span>
      );
      break;
    }
    case "timer_fired": {
      const timer = model.timers.find((t) => t.seq === e.seq);
      woke =
        timer?.timeoutOf != null ? (
          <>The timeout of the signal wait fired (timer #{e.seq}): the wait gave up</>
        ) : (
          <>
            Timer <b className="mono">#{e.seq}</b> fired: the{" "}
            {timer ? formatDuration(timer.durationMs) : ""} sleep is over
          </>
        );
      break;
    }
    case "signal_received": {
      const taken = model.received.find((r) => r.idx === step.first)?.waitIdx;
      woke = (
        <>
          Signal <b className="mono">"{e.name}"</b> arrived with{" "}
          <PayloadValue payload={e.payload} runId={ctx.runId} compact />
          {taken == null || taken > step.first ? (
            <span className="muted"> · it waits in the mailbox</span>
          ) : null}
        </>
      );
      break;
    }
    case "cancel_requested":
      woke =
        e.reason === "ttl" ? (
          <>Its deadline passed: the scheduler requested its cancellation</>
        ) : (
          <>Cancellation was requested{e.message ? <>: “{e.message}”</> : null}</>
        );
      break;
    default:
      woke = <span className="mono">{e.type}</span>;
  }

  // A cancellation rejects every pending await with CancelledError (archi/03-execution.md).
  const rejected = e.type === "cancel_requested" ? pendingBefore(model, step.first) : [];
  const did = step.committed.map((entry) => {
    const ev = entry.event;
    const key = entry.idx;
    const a = model.byIdx.get(entry.idx);
    switch (ev.type) {
      case "schedule_tool_call":
        return (
          <li key={key}>
            <span className="mk issued">{rank(entry.idx)}</span> called{" "}
            <button
              type="button"
              className="link-btn mono"
              onClick={() => onSelectEntry(entry.idx)}
            >
              #{ev.seq} {ev.tool}
            </button>
            {line(ev.callsite?.line)}
          </li>
        );
      case "start_timer":
        return a && isTimeout(a) ? (
          <li key={key}>
            <span className="mk">·</span> started timer #{ev.seq}, the{" "}
            {formatDuration(ev.durationMs)} timeout
            {line(ev.callsite?.line)}
          </li>
        ) : (
          <li key={key}>
            <span className="mk issued">{rank(entry.idx)}</span> slept{" "}
            {formatDuration(ev.durationMs)} (timer #{ev.seq}){line(ev.callsite?.line)}
          </li>
        );
      case "signal_awaited":
        return (
          <li key={key}>
            <span className="mk issued">{rank(entry.idx)}</span> awaited signal{" "}
            <b className="mono">"{ev.name}"</b>
            {line(ev.callsite?.line)}
          </li>
        );
      case "cancel_timer":
        return (
          <li key={key}>
            <span className="mk">·</span> cancelled timer #{ev.seq}
          </li>
        );
      case "log":
        return (
          <li key={key}>
            <span className="mk">·</span> logged <span className="mono">{ev.message}</span>
            {line(ev.callsite?.line)}
          </li>
        );
      case "complete_run":
        return (
          <li key={key} className="tone-good">
            <span className="mk">✓</span> returned{" "}
            <PayloadValue payload={ev.output} runId={ctx.runId} compact />
          </li>
        );
      case "fail_run": {
        const kind = step.ends === "cancelled" ? "cancelled" : "failed";
        return (
          <li key={key} className="step-end">
            <EndGlyph kind={kind} size={13} />
            <span>
              {kind === "cancelled" ? "ended cancelled with" : "threw"}{" "}
              <code className={`err-name end-${kind}`}>{ev.error.name}</code> {ev.error.message}
            </span>
          </li>
        );
      }
      case "run_aborted":
        return (
          <li key={key} className="step-end">
            <EndGlyph kind="aborted" size={13} />
            <span>
              was aborted by the host:{" "}
              <code className="err-name end-aborted">{ev.reason.kind}</code> {ev.reason.message}
            </span>
          </li>
        );
      default:
        return (
          <li key={key}>
            <span className="mk">·</span> <span className="mono">{ev.type}</span>
          </li>
        );
    }
  });

  const open = model.activities.filter(
    (a) => a.kind !== "log" && !isTimeout(a) && stateAt(a, step.last) === "open",
  );
  const paused = open.map((a) => (
    <li key={a.idx}>
      <span className="mk paused">‖</span>
      {a.kind === "tool_call" ? (
        <>
          <span className="mono">
            #{a.seq} {a.tool}
          </span>
          , in flight
        </>
      ) : a.kind === "timer" ? (
        <>sleep until {time(a.fireAt)}</>
      ) : a.kind === "signal" ? (
        <>
          signal <span className="mono">"{a.name}"</span>
          {a.timeoutAt !== null ? `, gives up ${time(a.timeoutAt)}` : ", no timeout"}
        </>
      ) : null}
      {line(a.callsite?.line)}
    </li>
  ));

  return (
    <section className="panel step-panel" aria-label="Step">
      <header className="panel-head">
        <h2>
          Step {cursor.n + 1} of {cursor.steps.length}
        </h2>
        {cursor.atHead && <span className="muted">the last</span>}
      </header>
      <div className="step-body">
        <div>
          <h4>Woke up because</h4>
          <div className="step-woke">{woke}</div>
        </div>
        <div>
          <h4>Then it</h4>
          <ul className="step-list">
            {rejected.map((p) => (
              <li key={`rejected-${p.activity.idx}`} className="step-end">
                <EndGlyph kind="cancelled" size={13} />
                <span>
                  the await of {p.what} threw{" "}
                  <code className="err-name end-cancelled">CancelledError</code>
                </span>
                {line(p.line ?? undefined)}
              </li>
            ))}
            {did.length > 0 || rejected.length > 0 ? (
              did
            ) : (
              <li className="muted">
                Nothing: the script still waits
                {open.length > 0 ? ` for ${open.map((a) => activityName(a)).join(", ")}` : ""}.
              </li>
            )}
          </ul>
        </div>
        <div>
          <h4>{step.ends ? "Ended" : "Now paused on"}</h4>
          <ul className="step-list">
            {step.ends ? (
              <li className="step-end">
                <EndGlyph kind={step.ends} size={13} />
                <span>
                  {ENDED[step.ends]} at{" "}
                  {time(Date.parse(model.entries[step.last]?.recordedAt ?? ""))}
                </span>
              </li>
            ) : paused.length > 0 ? (
              paused
            ) : (
              <li className="muted">Nothing open.</li>
            )}
          </ul>
        </div>
        <div>
          <h4>Watch</h4>
          {watches.length === 0 ? (
            <p className="muted watch-empty">Click a value in the code to watch it here.</p>
          ) : (
            <ul className="watches" aria-label="Watches">
              {watches.map((key) => {
                const target = targets.get(key);
                if (!target) return null;
                return (
                  <li key={key}>
                    <span className="watch-name mono">{targetName(target)}</span>
                    <span className="watch-value">
                      <WatchValue target={target} ctx={ctx} />
                    </span>
                    <button
                      type="button"
                      className="icon-btn"
                      aria-label={`Stop watching ${targetName(target)}`}
                      onClick={() => onUnwatch(key)}
                    >
                      <X size={12} />
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
          <p className="muted limit-note">
            Only values that crossed the journal are known: inputs, results, signals, and the
            variables bound to an awaited call. What the script computes in between is not recorded.
          </p>
        </div>
      </div>
    </section>
  );
}
