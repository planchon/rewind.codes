// How the run ended, told once at the top of its page: a cancellation says why (its deadline, a
// request, its code) and what the run awaited then; a failure says what threw, where. The
// error itself follows, its name apart from its message. The time cursor's bar says in which
// step it ended, with the same mark.

import { useSettings } from "../../app/settings";
import { EndGlyph } from "../../components/Status";
import {
  type CancelCause,
  cancelCause,
  causeText,
  type EndKind,
  endKindOf,
  endTitle,
  type Pending,
  pendingBefore,
} from "../../model/ending";
import { formatDuration, formatTime } from "../../model/format";
import type { RunModel } from "../../model/run";
import type { Run } from "../../model/types";

export interface Ending {
  kind: EndKind;
  title: string;
  cause: CancelCause | null;
  /** The awaits pending when the cancellation, or the end, came. */
  pending: Pending[];
  error: { name: string; message: string; stack: string | null } | null;
  at: number | null;
}

/** How the run ended, from its details and its history; `null` while it goes on. */
export function endingOf(run: Run, model: RunModel): Ending | null {
  const kind = endKindOf(run.status);
  if (!kind) return null;
  const cause =
    kind === "cancelled"
      ? cancelCause({
          cancel: model.cancel,
          historyRead: model.end !== null,
          createdAt: run.createdAt,
          expiresAt: run.expiresAt,
          endedAt: run.endedAt ?? null,
        })
      : null;
  const point = kind === "cancelled" ? (model.cancel?.idx ?? model.end?.idx) : model.end?.idx;
  const error = run.error
    ? { name: run.error.name, message: run.error.message, stack: run.error.stack ?? null }
    : run.abort
      ? { name: run.abort.kind, message: run.abort.message, stack: null }
      : null;
  return {
    kind,
    title: endTitle(kind, cause),
    cause,
    pending:
      (kind === "cancelled" || kind === "aborted") && point !== undefined
        ? pendingBefore(model, point)
        : [],
    error,
    at: run.endedAt ? Date.parse(run.endedAt) : null,
  };
}

/** The verb of an ending, for "Cancelled in this step". */
export const ENDED: Record<EndKind, string> = {
  completed: "Completed",
  failed: "Failed",
  cancelled: "Cancelled",
  aborted: "Aborted",
};

function LineLink({ line, onSelectLine }: { line: number; onSelectLine: (line: number) => void }) {
  return (
    <button type="button" className="link-btn mono line-link" onClick={() => onSelectLine(line)}>
      L{line}
    </button>
  );
}

/** What it awaited: `it awaited signal "notify" L25; #2 ask_user was in flight L7`. */
function PendingList({
  pending,
  onSelectLine,
}: {
  pending: Pending[];
  onSelectLine: (line: number) => void;
}) {
  return (
    <>
      {pending.map((p, i) => (
        <span key={p.activity.idx}>
          {i > 0 && "; "}
          {p.text}
          {p.line !== null && (
            <>
              {" "}
              <LineLink line={p.line} onSelectLine={onSelectLine} />
            </>
          )}
        </span>
      ))}
    </>
  );
}

/** A stack that says more than its first line. */
function frames(stack: string | null, name: string, message: string): string | null {
  if (!stack) return null;
  const trimmed = stack.trim();
  return trimmed === `${name}: ${message}` || trimmed === name ? null : stack;
}

export function RunEndBanner({
  run,
  ending,
  model,
  onSelectEvent,
  onSelectLine,
}: {
  run: Run;
  ending: Ending;
  model: RunModel;
  onSelectEvent: (idx: number) => void;
  onSelectLine: (line: number) => void;
}) {
  const { zone } = useSettings();
  const { kind, cause, pending, error } = ending;
  const e = run.error;
  const failedCall = e?.failedCall ?? null;
  const call = failedCall ? model.calls.find((c) => c.seq === failedCall.seq) : undefined;
  const why = cause ? causeText(cause) : null;
  const stack = error ? frames(error.stack, error.name, error.message) : null;
  const started = Date.parse(run.createdAt);
  return (
    <section className={`banner end-banner end-banner-${kind}`} aria-label="How the run ended">
      <div className="end-banner-head">
        <EndGlyph kind={kind} size={16} />
        <h2 className="end-banner-title">{ending.title}</h2>
        {ending.at !== null && (
          <span className="end-banner-when num">
            {formatTime(ending.at, zone)}
            {zone === "utc" ? " UTC" : ""} · after {formatDuration(ending.at - started)}
          </span>
        )}
      </div>
      {(why || pending.length > 0) && (
        <p className="end-banner-story">
          {why}
          {pending.length > 0 && (
            <>
              {why ? " " : ""}
              {kind === "cancelled" ? "When it came, " : "When the engine aborted it, "}
              <PendingList pending={pending} onSelectLine={onSelectLine} />.
            </>
          )}
        </p>
      )}
      {error && (
        <p className="end-banner-error">
          <code className={`err-name end-${kind}`}>{error.name}</code>
          <span className="end-banner-message">{error.message}</span>
        </p>
      )}
      {(e?.location || failedCall) && (
        <p className="end-banner-links">
          {e?.location && (
            <button
              type="button"
              className="link-btn mono"
              onClick={() => e.location && onSelectLine(e.location.line)}
            >
              workflow.ts:{e.location.line}:{e.location.column}
            </button>
          )}
          {failedCall && (
            <button
              type="button"
              className="link-btn"
              onClick={() => call && onSelectEvent(call.idx)}
              disabled={!call}
            >
              thrown by{" "}
              <span className="mono">
                #{failedCall.seq} {failedCall.action}
              </span>
              : {failedCall.error.kind}, {failedCall.error.message}
            </button>
          )}
        </p>
      )}
      {e?.frame && <pre className="frame">{e.frame}</pre>}
      {stack && (
        <details>
          <summary>Stack</summary>
          <pre className="frame">{stack}</pre>
        </details>
      )}
    </section>
  );
}
