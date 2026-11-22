// Two cells of the table of runs that abbreviate, each with a card that says it whole: a run's
// labels (those that fit, then "+N"), and how it ended (the error's name over its message).
import { useState } from "react";
import { Link } from "react-router";
import { useRunLinkState } from "../app/runLinks";
import { useSettings } from "../app/settings";
import { LabelList } from "../components/bits";
import { HoverCard } from "../components/HoverCard";
import { EndGlyph } from "../components/Status";
import { cancelCause, causeText, endKindOf, endTitle } from "../model/ending";
import {
  fitLabels,
  formatDuration,
  formatRelative,
  formatTime,
  labelPair,
  shortLabelValue,
} from "../model/format";
import type { Labels, RunSummary } from "../model/types";
import { useRun } from "../state/runs";

/** A run's labels in a cell: those that fit, whole and readable, then "+N" for the rest. */
export function RunLabels({
  labels,
  onPick,
}: {
  labels: Labels;
  onPick?: ((pair: string) => void) | undefined;
}) {
  const pairs = Object.entries(labels).sort(([a], [b]) => a.localeCompare(b));
  if (pairs.length === 0) return null;
  const { shown, hidden } = fitLabels(pairs);
  return (
    <span className="run-labels">
      {shown.map(([key, value]) => {
        const pair = labelPair(key, value);
        const body = (
          <>
            <span className="k">{key}:</span>
            {shortLabelValue(value)}
          </>
        );
        return onPick ? (
          <button
            key={pair}
            type="button"
            className="label"
            title={`${pair}: only the runs with this label`}
            onClick={(e) => {
              e.stopPropagation();
              onPick(pair);
            }}
          >
            {body}
          </button>
        ) : (
          <span key={pair} className="label" title={pair}>
            {body}
          </span>
        );
      })}
      {hidden > 0 && (
        <HoverCard
          label={`The run's ${pairs.length} labels`}
          className="labels-card"
          trigger={
            <button type="button" className="label more" aria-label={`${hidden} more labels`}>
              +{hidden}
            </button>
          }
        >
          <p className="card-title">{pairs.length} labels</p>
          <LabelList labels={labels} {...(onPick ? { onPick } : {})} />
          {onPick && <p className="card-meta">Click a label to see only the runs with it.</p>}
        </HoverCard>
      )}
    </span>
  );
}

/** How a run ended, in a cell: the error's name over its message; the card says the rest. */
export function RunEnd({ run }: { run: RunSummary }) {
  const [open, setOpen] = useState(false);
  const kind = endKindOf(run.status);
  if (!run.error || !kind) return null;
  return (
    <HoverCard
      label={`How the run ${run.id} ended`}
      className="end-card"
      onOpenChange={setOpen}
      trigger={
        <button
          type="button"
          className={`end-cell end-${kind}`}
          aria-label={`${run.error}${run.errorMessage ? `: ${run.errorMessage}` : ""}`}
        >
          <span className="end-cell-name">{run.error}</span>
          {run.errorMessage && <span className="end-cell-message">{run.errorMessage}</span>}
        </button>
      }
    >
      <EndCard run={run} open={open} />
    </HoverCard>
  );
}

function EndCard({ run, open }: { run: RunSummary; open: boolean }) {
  const { zone } = useSettings();
  const linkState = useRunLinkState();
  // The details say what the summary does not: the deadline, the line, the call that threw.
  const detail = useRun(run.id, open);
  const kind = endKindOf(run.status) ?? "failed";
  const d = detail.data;
  const cause =
    kind === "cancelled"
      ? cancelCause({
          createdAt: d?.createdAt ?? run.startedAt,
          expiresAt: d?.expiresAt ?? null,
          endedAt: run.endedAt ?? null,
        })
      : null;
  const why = cause ? causeText(cause) : null;
  const line = run.errorLine ?? d?.error?.location?.line ?? null;
  const failedCall = d?.error?.failedCall ?? null;
  const ended = run.endedAt ? Date.parse(run.endedAt) : null;
  return (
    <>
      <p className="card-title end-card-title">
        <EndGlyph kind={kind} />
        {endTitle(kind, cause)}
      </p>
      <p className="end-card-error">
        <code className={`err-name end-${kind}`}>{run.error}</code>
        {run.errorMessage && <span className="end-card-message">{run.errorMessage}</span>}
      </p>
      <dl className="facts">
        {kind === "cancelled" && (
          <>
            <dt>Cause</dt>
            <dd>{why ?? (detail.isPending ? "…" : "Not said: open the run for its journal.")}</dd>
          </>
        )}
        {failedCall && (
          <>
            <dt>Thrown by</dt>
            <dd>
              <span className="mono">
                #{failedCall.seq} {failedCall.tool}
              </span>
              : {failedCall.error.kind}, {failedCall.error.message}
            </dd>
          </>
        )}
        {line !== null && (
          <>
            <dt>Line</dt>
            <dd className="mono">script.ts:{line}</dd>
          </>
        )}
        {ended !== null && (
          <>
            <dt>Ended</dt>
            <dd className="num">
              {formatTime(ended, zone)}
              {zone === "utc" ? " UTC" : ""}
              <span className="card-meta">
                {" "}
                · {formatRelative(ended, Date.now())}, after{" "}
                {formatDuration(ended - Date.parse(run.startedAt))}
              </span>
            </dd>
          </>
        )}
      </dl>
      <Link className="card-link" to={`/runs/${run.id}`} state={linkState}>
        Open the run, at the step it ended
      </Link>
    </>
  );
}
