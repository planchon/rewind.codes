// Two cells of a list of runs, each with a card that says more: a run's labels (whole when few
// and short, else "6 labels"), and how it ended (the error's name over its message).
import { useState } from "react";
import { Link } from "react-router";
import { runPath, useRunLinkState } from "../app/runLinks";
import { useSettings } from "../app/settings";
import { HoverCard } from "../components/HoverCard";
import { RowLabels } from "../components/RowLabels";
import { EndGlyph } from "../components/Status";
import { cancelCause, causeText, endKindOf, endTitle } from "../model/ending";
import { formatDuration, formatRelative, formatTime } from "../model/format";
import type { RunSummary } from "../model/types";
import { useRun } from "../state/runs";

/** A run's labels, each a filter of the list. */
export function RunLabels({
  labels,
  onPick,
}: {
  labels: RunSummary["labels"];
  onPick?: ((pair: string) => void) | undefined;
}) {
  return (
    <RowLabels
      labels={labels}
      onPick={onPick}
      pickTitle={(pair) => `${pair}: only the runs with this label`}
      owner="The run's"
    />
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
      <Link className="card-link" to={runPath(run.handle)} state={linkState}>
        Open the run, at the step it ended
      </Link>
    </>
  );
}
