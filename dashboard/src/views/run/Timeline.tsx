// The run's timeline: one row per call, timer and signal wait, on an axis where long idle
// gaps are compressed into marked breaks. Open spans end at "now" while the run goes on, with
// their planned end (a timer's fire time, a wait's timeout) dashed beyond. Every row shows, with
// no scroll of its own: the page scrolls them, and only those near the window are drawn.
import { useWindowVirtualizer } from "@tanstack/react-virtual";
import { type PointerEvent, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTooltip } from "../../components/Tooltip";
import { formatDuration, formatOffset } from "../../model/format";
import type { Activity, RunModel } from "../../model/run";
import type { Step } from "../../model/steps";
import { buildTimeScale } from "../../model/timescale";
import { activityDuration, activityOutcome, activityTitle } from "./describe";

const ROW = 20;

interface Row {
  activity: Activity;
  start: number;
  end: number;
  planned: number | null;
}

export interface TimelineProps {
  model: RunModel;
  /** The current time while the run goes on; `null` once it has ended. */
  now: number | null;
  selected: number | null;
  onSelect: (idx: number) => void;
  /** The time cursor: its steps, the one shown, and how to move it. */
  cursor?: {
    steps: readonly Step[];
    n: number;
    /** The last event the cursor includes. */
    last: number;
    atHead: boolean;
    onSeek: (n: number) => void;
  };
}

export function Timeline({ model, now, selected, onSelect, cursor }: TimelineProps) {
  const list = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);
  const tip = useTooltip();

  const { rows, scale, logs, endAt } = useMemo(() => {
    const endAt = now ?? model.end?.at ?? model.lastAt ?? 0;
    const rows: Row[] = [];
    const times: number[] = [];
    for (const e of model.entries) times.push(Date.parse(e.recordedAt));
    if (now !== null) times.push(now);
    for (const a of model.activities) {
      if (a.kind === "log") continue;
      // A wait's timeout shows on the wait's own row.
      if (a.kind === "timer" && a.timeoutOf !== null) continue;
      const planned = a.end
        ? null
        : a.kind === "timer"
          ? a.fireAt
          : a.kind === "signal"
            ? a.timeoutAt
            : null;
      if (planned !== null && now !== null) times.push(planned);
      rows.push({ activity: a, start: a.at, end: a.end?.at ?? endAt, planned });
    }
    return { rows, scale: buildTimeScale(times), logs: model.logs, endAt };
  }, [model, now]);

  const top = useListTop(list, rows.length > 0);
  const virtual = useWindowVirtualizer({
    count: rows.length,
    estimateSize: () => ROW,
    overscan: 12,
    scrollMargin: top,
    initialRect: { width: 800, height: 800 },
  });

  if (rows.length === 0 && logs.length === 0) {
    return <div className="empty">Nothing happened yet: no call, timer or signal wait.</div>;
  }

  const pct = (t: number) => `${(scale.x(t) * 100).toFixed(3)}%`;
  const t0 = scale.t0;
  const step = cursor?.steps[cursor.n];
  const playhead = step ? scale.x(step.at) : null;
  const future = (a: Activity) => cursor !== undefined && !cursor.atHead && a.idx > cursor.last;

  /** The step under the pointer: the last one at or before it. */
  const seek = (e: PointerEvent<HTMLElement>) => {
    if (!cursor) return;
    const rect = e.currentTarget.getBoundingClientRect();
    if (rect.width <= 0) return;
    const x = (e.clientX - rect.left) / rect.width;
    let n = 0;
    cursor.steps.forEach((s, i) => {
      if (scale.x(s.at) <= x + 0.004) n = i;
    });
    if (n !== cursor.n) cursor.onSeek(n);
  };

  const tipFor = (r: Row) => {
    const a = r.activity;
    const d = activityDuration(a, endAt);
    return (
      <>
        <div className="tt-title">{activityTitle(a)}</div>
        <dl>
          <dt>outcome</dt>
          <dd>{activityOutcome(a, model.end !== null).replace("_", " ")}</dd>
          <dt>start</dt>
          <dd>{formatOffset(r.start - t0)}</dd>
          {d !== null && (
            <>
              <dt>{a.kind !== "log" && a.end ? "took" : "for"}</dt>
              <dd>{formatDuration(d)}</dd>
            </>
          )}
          {r.planned !== null && (
            <>
              <dt>{a.kind === "timer" ? "fires" : "times out"}</dt>
              <dd>{formatOffset(r.planned - t0)}</dd>
            </>
          )}
          {a.callsite && (
            <>
              <dt>line</dt>
              <dd>
                {a.callsite.line}:{a.callsite.column}
              </dd>
            </>
          )}
        </dl>
      </>
    );
  };

  return (
    <div className="timeline">
      <div className="tl-axis">
        <div className="tl-label muted">{rows.length} spans</div>
        <div
          className={`tl-track${cursor ? " seekable" : ""}`}
          onPointerDown={
            cursor
              ? (e) => {
                  dragging.current = true;
                  e.currentTarget.setPointerCapture?.(e.pointerId);
                  seek(e);
                }
              : undefined
          }
          onPointerMove={cursor ? (e) => dragging.current && seek(e) : undefined}
          onPointerUp={
            cursor
              ? (e) => {
                  dragging.current = false;
                  e.currentTarget.releasePointerCapture?.(e.pointerId);
                }
              : undefined
          }
        >
          {cursor && playhead !== null && (
            <span
              className={`tl-playhead${cursor.atHead ? " head" : ""}`}
              style={{ left: `${playhead * 100}%` }}
              role="slider"
              tabIndex={0}
              aria-label="Time cursor: drag it, or use the arrow keys"
              aria-valuemin={1}
              aria-valuemax={cursor.steps.length}
              aria-valuenow={cursor.n + 1}
              aria-valuetext={`Step ${cursor.n + 1} of ${cursor.steps.length}`}
            />
          )}
          {scale.ticks.map((t) => (
            <span
              key={`${t.t}-${t.major}`}
              className={`tl-tick${t.major ? " major" : ""}${t.x > 0.93 ? " end" : ""}`}
              style={{ left: pct(t.t) }}
            >
              {t.label}
            </span>
          ))}
          {scale.breaks.map((b) => (
            <span
              key={b.from}
              className={`tl-break-label${b.x1 >= 0.999 ? " end" : ""}`}
              style={{ left: `${(b.x1 >= 0.999 ? 1 : (b.x0 + b.x1) / 2) * 100}%` }}
              title={`${formatDuration(b.to - b.from)} with nothing happening, compressed`}
            >
              ⋯{formatDuration(b.to - b.from).split(" ")[0]}
            </span>
          ))}
        </div>
      </div>
      {logs.length > 0 && (
        <div className="tl-row tl-logs">
          <div className="tl-label muted">logs</div>
          <div className="tl-track">
            {logs.map((l) => (
              <button
                key={l.idx}
                type="button"
                className={`tl-log level-${l.level}${selected === l.idx ? " selected" : ""}${future(l) ? " future" : ""}`}
                style={{ left: pct(l.at) }}
                aria-label={`console.${l.level}: ${l.message}`}
                onMouseMove={(e) =>
                  tip.show(
                    e,
                    <>
                      <div className="tt-title">
                        console.{l.level} · {formatOffset(l.at - t0)}
                      </div>
                      {l.message}
                    </>,
                  )
                }
                onMouseLeave={tip.hide}
                onClick={() => onSelect(l.idx)}
              />
            ))}
          </div>
        </div>
      )}
      <div className="tl-rows" ref={list}>
        <div style={{ height: virtual.getTotalSize(), position: "relative" }}>
          {scale.breaks.map((b) => (
            <span
              key={b.from}
              className="tl-break"
              style={{
                left: `calc(var(--tl-label) + (100% - var(--tl-label)) * ${b.x0})`,
                width: `calc((100% - var(--tl-label)) * ${b.x1 - b.x0})`,
              }}
              aria-hidden="true"
            />
          ))}
          {cursor && playhead !== null && !cursor.atHead && (
            <span
              className="tl-cursor"
              style={{
                left: `calc(var(--tl-label) + (100% - var(--tl-label)) * ${playhead})`,
              }}
              aria-hidden="true"
            />
          )}
          {now !== null && (
            <span
              className="tl-now"
              style={{ left: `calc(var(--tl-label) + (100% - var(--tl-label)) * ${scale.x(now)})` }}
              aria-hidden="true"
            />
          )}
          {virtual.getVirtualItems().map((item) => {
            const r = rows[item.index];
            if (!r) return null;
            const a = r.activity;
            const outcome = activityOutcome(a, model.end !== null);
            const open = outcome === "open";
            const left = scale.x(r.start);
            const right = scale.x(r.end);
            return (
              <div
                key={a.idx}
                className={`tl-row${selected === a.idx ? " selected" : ""}${future(a) ? " future" : ""}`}
                style={{
                  position: "absolute",
                  top: item.start - top,
                  left: 0,
                  right: 0,
                  height: ROW,
                }}
              >
                <button
                  type="button"
                  className="tl-label"
                  onClick={() => onSelect(a.idx)}
                  title={activityTitle(a)}
                >
                  <span className="mono">{activityTitle(a)}</span>
                  {a.callsite && <span className="muted"> L{a.callsite.line}</span>}
                </button>
                <div className="tl-track">
                  {r.planned !== null && (
                    <span
                      className="tl-planned"
                      style={{
                        left: pct(r.end),
                        width: `${Math.max(0, scale.x(r.planned) - right) * 100}%`,
                      }}
                    />
                  )}
                  <button
                    type="button"
                    className={`tl-bar kind-${a.kind} outcome-${outcome}${open ? " open" : ""}`}
                    style={{ left: `${left * 100}%`, width: `max(3px, ${(right - left) * 100}%)` }}
                    aria-label={`${activityTitle(a)}: ${outcome}`}
                    onMouseMove={(e) => tip.show(e, tipFor(r))}
                    onMouseLeave={tip.hide}
                    onClick={() => onSelect(a.idx)}
                  />
                </div>
              </div>
            );
          })}
        </div>
      </div>
      <div className="tl-legend">
        <span>
          <i className="sw kind-call" /> call
        </span>
        <span>
          <i className="sw kind-call outcome-failed" /> failed
        </span>
        <span>
          <i className="sw kind-timer" /> timer
        </span>
        <span>
          <i className="sw kind-signal" /> signal wait
        </span>
        <span>
          <i className="sw open-sw" /> still open
        </span>
        {scale.breaks.length > 0 && (
          <span>
            <i className="sw break-sw" /> idle time, compressed
          </span>
        )}
      </div>
      {tip.view}
    </div>
  );
}

/** Where the list starts in the page, which the window's virtualizer counts its rows from; again
 * whenever the page above it changes size. */
function useListTop(list: { current: HTMLElement | null }, shown: boolean): number {
  const [top, setTop] = useState(0);
  useLayoutEffect(() => {
    const el = list.current;
    if (!shown || !el) return;
    const measure = () => setTop(Math.round(el.getBoundingClientRect().top + window.scrollY));
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(document.body);
    return () => observer.disconnect();
  }, [list, shown]);
  return top;
}
