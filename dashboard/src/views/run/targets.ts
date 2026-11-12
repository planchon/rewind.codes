// What can be hovered in a run's source: each call expression that caused activities (calls,
// timers, signal waits), the names bound to their awaited results, and the run's input. Built
// from the source's syntax tree and the run's activities; the time cursor decides what they show.
import type { LineTarget } from "../../components/CodeView";
import {
  type Binding,
  bindingsOf,
  type CallSpan,
  calleeNames,
  indexSource,
  inputBindings,
  resolveCall,
  type SourceIndex,
  type Span,
  startLine,
} from "../../model/callsites";
import type { Activity, RunModel } from "../../model/run";
import { isTimeout } from "../../model/steps";

export interface CallTarget {
  kind: "call";
  key: string;
  span: CallSpan;
  /** The text of the call, for a watch's name. */
  text: string;
  /** What it caused, in order: several in a loop or a fan-out. */
  activities: Activity[];
}

export interface BindingTarget {
  kind: "binding";
  key: string;
  name: string;
  /** The field of the result it is bound to. */
  path: string | number | null;
  call: CallTarget;
  refs: Span[];
}

export interface InputTarget {
  kind: "input";
  key: string;
  name: string;
  path: string | number | null;
  refs: Span[];
}

export type Target = CallTarget | BindingTarget | InputTarget;

export interface Targets {
  byKey: Map<string, Target>;
  /** The targets of each line, without overlaps: the innermost wins. */
  byLine: Map<number, LineTarget[]>;
  /** The line the run starts at. */
  startLine: number | null;
}

export function sourceIndex(source: string): SourceIndex | null {
  try {
    return indexSource(source);
  } catch {
    return null;
  }
}

interface Range {
  from: number;
  to: number;
  key: string;
  kind: string;
  size: number;
}

/** Cuts ranges at line ends, then keeps the innermost range under each character. */
function lineTargets(index: SourceIndex, ranges: Range[]): Map<number, LineTarget[]> {
  const perLine = new Map<number, Range[]>();
  for (const r of ranges) {
    for (let line = 1; line <= index.lineStarts.length; line += 1) {
      const start = index.lineStarts[line - 1] ?? 0;
      const end = index.lineStarts[line] ?? index.text.length + 1;
      if (r.to <= start || r.from >= end) continue;
      const list = perLine.get(line) ?? [];
      list.push({
        ...r,
        from: Math.max(r.from, start) - start,
        to: Math.min(r.to, end - 1) - start,
      });
      perLine.set(line, list);
    }
  }
  const out = new Map<number, LineTarget[]>();
  for (const [line, list] of perLine) {
    const edges = [...new Set(list.flatMap((r) => [r.from, r.to]))].sort((a, b) => a - b);
    const pieces: LineTarget[] = [];
    for (let i = 0; i + 1 < edges.length; i += 1) {
      const from = edges[i] ?? 0;
      const to = edges[i + 1] ?? from;
      if (to <= from) continue;
      const owner = list
        .filter((r) => r.from <= from && to <= r.to)
        .sort((a, b) => a.size - b.size)[0];
      if (!owner) continue;
      const last = pieces[pieces.length - 1];
      if (last && last.key === owner.key && last.to === from) last.to = to;
      else pieces.push({ from, to, key: owner.key, kind: owner.kind });
    }
    out.set(line, pieces);
  }
  return out;
}

export function buildTargets(index: SourceIndex, model: RunModel): Targets {
  const byKey = new Map<string, Target>();
  const ranges: Range[] = [];
  const range = (span: Span, key: string, kind: string) =>
    ranges.push({ from: span.from, to: span.to, key, kind, size: span.to - span.from });

  for (const a of model.activities) {
    if (a.kind === "log" || isTimeout(a) || !a.callsite) continue;
    const span = resolve(index, a);
    if (!span) continue;
    const key = `call:${span.from}`;
    const existing = byKey.get(key);
    if (existing?.kind === "call") {
      existing.activities.push(a);
      continue;
    }
    const call: CallTarget = {
      kind: "call",
      key,
      span,
      text: index.text.slice(span.from, span.to),
      activities: [a],
    };
    byKey.set(key, call);
    range(span, key, "call");
    for (const b of bindingsOf(index, span)) addBinding(b, call);
  }

  function addBinding(b: Binding, call: CallTarget) {
    const key = `bind:${b.def.from}`;
    byKey.set(key, { kind: "binding", key, name: b.name, path: b.path, call, refs: b.refs });
    for (const ref of b.refs) range(ref, key, "binding");
  }

  for (const b of inputBindings(index)) {
    const key = `input:${b.def.from}`;
    byKey.set(key, { kind: "input", key, name: b.name, path: b.path, refs: b.refs });
    for (const ref of b.refs) range(ref, key, "input");
  }

  return { byKey, byLine: lineTargets(index, ranges), startLine: startLine(index) };
}

/** The call an activity's callsite points at, cached per callsite. */
function resolve(index: SourceIndex, a: Activity): CallSpan | null {
  if (!a.callsite || a.kind === "log") return null;
  const cache = callCache.get(index) ?? new Map<string, CallSpan | null>();
  callCache.set(index, cache);
  const names = calleeNames(a);
  const key = `${a.callsite.line}:${a.callsite.column}:${names.join(",")}`;
  if (!cache.has(key)) cache.set(key, resolveCall(index, a.callsite, names));
  return cache.get(key) ?? null;
}

const callCache = new WeakMap<SourceIndex, Map<string, CallSpan | null>>();
