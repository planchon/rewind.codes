// Run statuses and call outcomes: always a glyph and a word, never color alone.
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
import type { RunStatus } from "../api/types";

const STATUS_ICON = {
  completed: CircleCheck,
  failed: CircleX,
  aborted: OctagonX,
  cancelled: Ban,
  active: CircleDot,
  pending: CircleDashed,
} as const;

export function StatusGlyph({ status, size = 13 }: { status: RunStatus; size?: number }) {
  const Icon = STATUS_ICON[status];
  return (
    <span className={`glyph${status === "active" ? " pulse" : ""}`} aria-hidden="true">
      <Icon size={size} strokeWidth={2.25} />
    </span>
  );
}

export function StatusBadge({ status, large = false }: { status: RunStatus; large?: boolean }) {
  return (
    <span className={`status ${status}${large ? " large" : ""}`}>
      <StatusGlyph status={status} size={large ? 15 : 13} />
      {status}
    </span>
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

export function MockTag() {
  return (
    <span
      className="tag info"
      title="Mock mode: tools answered by cassettes and mocks, time is virtual"
    >
      <span className="glyph" aria-hidden="true">
        <FlaskConical size={11} strokeWidth={2.5} />
      </span>
      mock
    </span>
  );
}

export function ForkTag({ children }: { children?: ReactNode }) {
  return (
    <span className="tag" title="A fork: started from another run with a corrected script">
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
