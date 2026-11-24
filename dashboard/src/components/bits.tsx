// Small shared pieces: copy buttons, ids and digests, labels, times, problems.
import { Check, Copy, Link2, TriangleExclamationPoint } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { Link } from "react-router";
import { useSettings } from "../app/settings";
import {
  formatRelative,
  formatTime,
  isIdValue,
  labelPair,
  shortDigest,
  shortRunId,
} from "../model/format";
import { labelTerm, queryLink } from "../model/query";
import type { Labels } from "../model/types";
import { ApiError } from "../state/errors";
import { useNamespaceLabel } from "./NamespaceName";

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
 * namespace dimmed, by its display name when it has one (left out where a column of the page says
 * it; a long id shortened out of a table), the suffix in mono; a button copies the handle whole,
 * with the namespace's id, and the title gives it and the run's UUID. A run named by its UUID only
 * (one erased since) shows as an id.
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
  const displayName = useNamespaceLabel();
  const parts = handle.split("/");
  if (parts.length !== 3)
    return copy ? <RunId id={handle} /> : <span className="run-id">{handle}</span>;
  const [ns = "", origin = "", suffix = ""] = parts;
  const shown = displayName(ns);
  return (
    <span className="copyable">
      <span className="run-handle" title={id ? `${handle}\nid ${id}` : handle}>
        {namespace && (
          <>
            <span className="h-ns">{shown ?? (whole ? ns : shortNamespace(ns))}</span>
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
  pickTitle = (pair) => `Filter by ${pair}`,
  counts,
}: {
  labels: Labels | string[];
  onPick?: ((pair: string) => void) | undefined;
  /** What picking a label does, in its title. */
  pickTitle?: (pair: string) => string;
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
        const value = pair.slice(at + 1);
        if (at > 0 && isIdValue(value))
          return <IdLabel key={pair} name={pair.slice(0, at)} id={value} />;
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
            title={pickTitle(pair)}
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

/**
 * A label whose value is an id (`goal:6afbb6ae…`): its key and a link's icon, never the id, which
 * its title gives; it opens every run with the label.
 */
export function IdLabel({ name, id, count }: { name: string; id: string; count?: string }) {
  return (
    <Link
      to={queryLink("/observability", labelTerm(name, id))}
      className="label label-id"
      title={`${name}:${id}\nEvery run with this label`}
      onClick={(e) => e.stopPropagation()}
    >
      {name}
      <Link2 size={12} aria-hidden="true" />
      {count !== undefined && <span className="count">{count}</span>}
    </Link>
  );
}

/** A value once it has stopped changing for `ms`: what is typed, asked for once. */
export function useSettled<T>(value: T, ms = 150): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(timer);
  }, [value, ms]);
  return settled;
}

/**
 * The current time, updated every `everyMs`, for relative times and countdowns: a clock that reads
 * no store (archi/07, principle 11), drawing again what is already on screen.
 */
export function useNow(everyMs = 15_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    // biome-ignore lint/plugin/no-polling: a display clock; it reads no store, only `Date.now()`.
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
