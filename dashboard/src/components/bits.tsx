// Small shared pieces: copy buttons, ids and digests, labels, times, problems.
import { Check, Copy, TriangleExclamationPoint } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { useSettings } from "../app/settings";
import { formatRelative, formatTime, labelPair, shortDigest, shortRunId } from "../model/format";
import type { Labels } from "../model/types";
import { ApiError } from "../state/errors";

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  useEffect(() => {
    if (!done) return;
    const t = setTimeout(() => setDone(false), 1_200);
    return () => clearTimeout(t);
  }, [done]);
  return (
    <button
      type="button"
      className={`copy-btn${done ? " done" : ""}`}
      aria-label={label}
      title={label}
      onClick={(e) => {
        e.stopPropagation();
        e.preventDefault();
        void navigator.clipboard?.writeText(text).then(
          () => setDone(true),
          () => undefined,
        );
      }}
    >
      {done ? <Check size={12} /> : <Copy size={12} />}
    </button>
  );
}

export function Digest({
  digest,
  full = false,
  onPick,
  pickTitle,
}: {
  digest: string;
  full?: boolean;
  /** Makes the digest a button beside its copy button, never around it. */
  onPick?: () => void;
  pickTitle?: string;
}) {
  const text = full ? digest : shortDigest(digest);
  return (
    <span className="copyable">
      {onPick ? (
        <button
          type="button"
          className="link-btn digest"
          title={pickTitle ?? digest}
          onClick={(e) => {
            e.stopPropagation();
            onPick();
          }}
        >
          {text}
        </button>
      ) : (
        <span className="digest" title={digest}>
          {text}
        </span>
      )}
      <CopyButton text={digest} label="Copy digest" />
    </span>
  );
}

/** A namespace's id cut in its middle when long: `6afbb6…77f1`. */
function shortNamespace(name: string): string {
  return name.length > 16 ? `${name.slice(0, 6)}…${name.slice(-4)}` : name;
}

/**
 * A run by its handle, `namespace/origin/suffix` (archi/06, "Run handles"): what ran in full, the
 * namespace dimmed (left out where a column of the page says it, a long id shortened out of a
 * table), the suffix in mono; a button copies the handle whole, and the title gives the run's UUID
 * too. A run named by its UUID only (one erased since) shows as an id.
 */
export function RunHandle({
  handle,
  id,
  namespace = true,
  copy = true,
  whole = false,
}: {
  handle: string;
  id?: string;
  namespace?: boolean;
  /** A copy button beside it: not inside a link, whose name it would join. */
  copy?: boolean;
  /** Its namespace whole, as in a table, where nothing is cut. */
  whole?: boolean;
}) {
  const parts = handle.split("/");
  if (parts.length !== 3)
    return copy ? <RunId id={handle} /> : <span className="run-id">{handle}</span>;
  const [ns = "", origin = "", suffix = ""] = parts;
  return (
    <span className="copyable">
      <span className="run-handle" title={id ? `${handle}\nid ${id}` : handle}>
        {namespace && (
          <>
            <span className="h-ns">{whole ? ns : shortNamespace(ns)}</span>
            <span className="h-sep">/</span>
          </>
        )}
        <span className="h-origin">{origin}</span>
        <span className="h-sep">/</span>
        <span className="h-suffix">{suffix}</span>
      </span>
      {copy && <CopyButton text={handle} label="Copy the run's handle" />}
    </span>
  );
}

export function RunId({ id, full = false }: { id: string; full?: boolean }) {
  return (
    <span className="copyable">
      <span className="run-id" title={id}>
        {full ? id : shortRunId(id)}
      </span>
      <CopyButton text={id} label="Copy run id" />
    </span>
  );
}

export function LabelList({
  labels,
  onPick,
  counts,
}: {
  labels: Labels | string[];
  onPick?: (pair: string) => void;
  counts?: Record<string, number>;
}) {
  const pairs = Array.isArray(labels)
    ? labels
    : Object.entries(labels)
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([k, v]) => labelPair(k, v));
  if (pairs.length === 0) return <span className="muted">–</span>;
  return (
    <span className="labels">
      {pairs.map((pair) => {
        const at = pair.indexOf(":");
        const body = (
          <>
            <span className="k">{pair.slice(0, at + 1)}</span>
            {pair.slice(at + 1)}
            {counts?.[pair] !== undefined && <span className="count">{counts[pair]}</span>}
          </>
        );
        return onPick ? (
          <button
            key={pair}
            type="button"
            className="label"
            title={`Filter by ${pair}`}
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
    </span>
  );
}

/** The current time, updated every `everyMs`, for relative times and countdowns. */
/** A value once it has stopped changing for `ms`: what is typed, asked for once. */
export function useSettled<T>(value: T, ms = 150): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(timer);
  }, [value, ms]);
  return settled;
}

export function useNow(everyMs = 15_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), everyMs);
    return () => clearInterval(t);
  }, [everyMs]);
  return now;
}

export function Time({ ms, withMs = false }: { ms: number; withMs?: boolean }) {
  const { zone } = useSettings();
  const text = formatTime(ms, zone, withMs);
  return (
    <time
      className="num nowrap"
      dateTime={new Date(ms).toISOString()}
      title={new Date(ms).toISOString()}
    >
      {text}
      {zone === "utc" && <span className="muted"> UTC</span>}
    </time>
  );
}

/** A relative time ("5h ago"); the exact time, in UTC and local, on hover. */
export function Ago({ ms }: { ms: number }) {
  const now = useNow();
  return (
    <time
      className="num nowrap"
      dateTime={new Date(ms).toISOString()}
      title={`${formatTime(ms, "utc")} UTC · ${formatTime(ms, "local")} local`}
    >
      {formatRelative(ms, now)}
    </time>
  );
}

export function ProblemView({ error, title }: { error: unknown; title?: string }) {
  const api = error instanceof ApiError ? error : null;
  const heading = title ?? api?.problem.title ?? "Something went wrong";
  const detail = api ? api.problem.detail : error instanceof Error ? error.message : String(error);
  return (
    <div className="problem" role="alert">
      <h3>
        <span className="glyph tone-critical" aria-hidden="true">
          <TriangleExclamationPoint size={14} />
        </span>
        {heading}
        {api && (
          <span className="slug">
            {api.status} · {api.slug}
          </span>
        )}
      </h3>
      {detail && <p>{detail}</p>}
    </div>
  );
}

export function Loading({ children }: { children?: ReactNode }) {
  return (
    <div className="loading">
      <span className="spinner" /> {children ?? "Loading…"}
    </div>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="empty">{children}</div>;
}
