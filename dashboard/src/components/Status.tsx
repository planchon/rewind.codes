// Run statuses and call outcomes: always a mark and a word, never color alone. A waiting run's
// status says `waiting`; what it waits for shows on hover (archi/08, "Waiting is a status").
import {
  Ban,
  CircleCheck,
  CircleDashed,
  CircleDot,
  CircleX,
  FlaskConical,
  GitFork,
  OctagonX,
  TriangleExclamationPoint,
} from "lucide-react";
import type { ReactNode } from "react";
import type { EndKind } from "../model/ending";
import type { RunStatus, RunWait } from "../model/types";
import { statusDetail } from "../model/waits";
import { useNow } from "./bits";
import { HoverCard } from "./HoverCard";

/**
 * A run's status: a dot in the status's color, then its word (Geist's status dot). A running run's
 * dot breathes; a waiting run's is a ring, still: it waits on the outside world.
 */
export function StatusGlyph({
  status,
  size = 10,
}: {
  /** A status, or `active` for the runs of a count that have not ended. */
  status: RunStatus | "active";
  size?: number;
}) {
  const d = Math.max(6, Math.min(10, size - 2));
  return (
    <span
      className={`status-dot${status === "running" ? " live" : ""}`}
      style={{ width: d, height: d }}
      aria-hidden="true"
    />
  );
}

export function StatusBadge({ status, large = false }: { status: RunStatus; large?: boolean }) {
  return (
    <span className={`status ${status}${large ? " large" : ""}`}>
      <StatusGlyph status={status} size={large ? 12 : 10} />
      <span className="status-word">{status}</span>
    </span>
  );
}

/**
 * A run's status in a table: its chip and, for a waiting run, a card on hover, focus or tap that
 * says what it waits for (`Waiting · signal approval, times out in 2d 23h`).
 */
export function RunStatusChip({
  status,
  waitingFor,
}: {
  status: RunStatus;
  waitingFor?: readonly RunWait[] | null | undefined;
}) {
  const waits = status === "waiting" ? (waitingFor ?? []) : [];
  if (waits.length === 0) return <StatusBadge status={status} />;
  return <WaitingStatus status={status} waits={waits} />;
}

function WaitingStatus({ status, waits }: { status: RunStatus; waits: readonly RunWait[] }) {
  const now = useNow(30_000);
  const lines = statusDetail(status, waits, now);
  return (
    <HoverCard
      label="What the run waits for"
      className="status-card"
      trigger={
        <button type="button" className="status-trigger" aria-label={lines.join("; ")}>
          <StatusBadge status={status} />
        </button>
      }
    >
      <ul className="status-card-lines">
        {lines.map((line) => (
          <li key={line}>{line}</li>
        ))}
      </ul>
    </HoverCard>
  );
}

export function QuarantineTag({ title }: { title?: string }) {
  return (
    <span
      className="tag warning"
      title={title ?? "Quarantined: the run is held until an operator releases or gives it up"}
    >
      <span className="glyph" aria-hidden="true">
        <TriangleExclamationPoint size={11} strokeWidth={2.5} />
      </span>
      quarantined
    </span>
  );
}

export function TestRunTag() {
  return (
    <span
      className="tag info"
      title="A test run: its calls answered by a test's world, in virtual time"
    >
      <span className="glyph" aria-hidden="true">
        <FlaskConical size={11} strokeWidth={2.5} />
      </span>
      test run
    </span>
  );
}

export function ForkTag({ children }: { children?: ReactNode }) {
  return (
    <span className="tag" title="A fork: started from another run with corrected code">
      <span className="glyph" aria-hidden="true">
        <GitFork size={11} strokeWidth={2.5} />
      </span>
      {children ?? "fork"}
    </span>
  );
}

export type Outcome = "ok" | "failed" | "open" | "fired" | "cancelled" | "received" | "timed_out";

const OUTCOME = {
  ok: { tone: "tone-good", word: "ok", Icon: CircleCheck },
  fired: { tone: "tone-good", word: "fired", Icon: CircleCheck },
  received: { tone: "tone-good", word: "received", Icon: CircleCheck },
  failed: { tone: "tone-critical", word: "failed", Icon: CircleX },
  timed_out: { tone: "tone-neutral", word: "timed out", Icon: CircleDashed },
  cancelled: { tone: "tone-neutral", word: "cancelled", Icon: Ban },
  open: { tone: "tone-info", word: "waiting", Icon: CircleDot },
} as const;

export function OutcomeBadge({ outcome, word }: { outcome: Outcome; word?: string }) {
  const o = OUTCOME[outcome];
  return (
    <span className="status">
      <span className={`glyph ${o.tone}${outcome === "open" ? " pulse" : ""}`} aria-hidden="true">
        <o.Icon size={12} strokeWidth={2.25} />
      </span>
      {word ?? o.word}
    </span>
  );
}

export function OutcomeGlyph({ outcome, size = 11 }: { outcome: Outcome; size?: number }) {
  const o = OUTCOME[outcome];
  return (
    <span
      className={`glyph ${o.tone}${outcome === "open" ? " pulse" : ""}`}
      role="img"
      aria-label={o.word}
    >
      <o.Icon size={size} strokeWidth={2.5} />
    </span>
  );
}

const END = {
  completed: { tone: "tone-good", Icon: CircleCheck },
  failed: { tone: "tone-critical", Icon: CircleX },
  cancelled: { tone: "tone-neutral", Icon: Ban },
  aborted: { tone: "tone-critical", Icon: OctagonX },
} as const;

/** The mark of how a run ended: a cancellation is gray, never red. */
export function EndGlyph({ kind, size = 14 }: { kind: EndKind; size?: number }) {
  const e = END[kind];
  return (
    <span className={`glyph ${e.tone}`} aria-hidden="true">
      <e.Icon size={size} strokeWidth={2.25} />
    </span>
  );
}
