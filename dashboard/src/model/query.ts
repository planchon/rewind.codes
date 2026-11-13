// The query language of the runs (archi/05-testing-and-observability.md, "Query language"), as the
// dashboard handles it: terms and where they are, a facet's value added, excluded or removed,
// suggestions as the operator types, and the filters of older links turned into a query. The
// server reads and checks every query (crates/rewind-api/src/query.rs): nothing here refuses one.
import type { Facets, RunStatus, WaitKind } from "../api/types";
import { RUN_STATUSES, WAIT_KINDS } from "../api/types";

export interface FieldSpec {
  name: string;
  /** What it matches, for the suggestions. */
  doc: string;
}

/** The fields of the language, in the order the server lists them. */
export const FIELDS: readonly FieldSpec[] = [
  { name: "status", doc: "status:failed,aborted" },
  { name: "tool", doc: "called a tool: tool:github.close_issue, tool:github.*" },
  { name: "error", doc: "a call's error kind or the run's error name" },
  { name: "waiting", doc: "active and waiting: waiting:*, waiting:signal" },
  { name: "signal", doc: "waiting for this signal" },
  { name: "started", doc: "started:>now-24h, started:2026-11-01" },
  { name: "ended", doc: "ended:<now-1h" },
  { name: "activity", doc: "last activity: activity:>now-1h" },
  { name: "duration", doc: "ended runs: duration:>5m" },
  { name: "calls", doc: "tool calls: calls:>=20" },
  { name: "errors", doc: "failed calls: errors:>0" },
  { name: "script", doc: "a script digest, 8 hex digits at least" },
  { name: "toolset", doc: "a toolset digest, 8 hex digits at least" },
  { name: "id", doc: "a run id prefix, 8 hex digits at least" },
  { name: "parent", doc: "forks of this run" },
  { name: "fingerprint", doc: "a failure group" },
  { name: "quarantined", doc: "quarantined:yes or no" },
  { name: "mode", doc: "mode:live or mock" },
];

const FIELD_NAMES = new Set(FIELDS.map((f) => f.name));

export type TermKind = "label" | "field" | "text";

export interface QueryTerm {
  /** Where the term is, in UTF-16 code units, `end` excluded. */
  start: number;
  end: number;
  raw: string;
  /** Written with a leading `-`. */
  negated: boolean;
  /** The term without its `-`. */
  body: string;
  kind: TermKind;
  /** The field (`status`), the label's key (`agent`), or `null` for text. */
  field: string | null;
  /** What follows the colon, as written. */
  operand: string;
}

/** The terms of a query: runs of characters that are not spaces, spaces inside quotes kept. */
export function splitTerms(text: string): QueryTerm[] {
  const terms: QueryTerm[] = [];
  let start = -1;
  let quoted = false;
  let escaped = false;
  const push = (end: number) => {
    if (start >= 0) terms.push(readTerm(text, start, end));
    start = -1;
  };
  for (let at = 0; at < text.length; at += 1) {
    const c = text[at] ?? "";
    if (quoted) {
      if (escaped) escaped = false;
      else if (c === "\\") escaped = true;
      else if (c === '"') quoted = false;
      continue;
    }
    if (/\s/.test(c)) {
      push(at);
      continue;
    }
    if (start < 0) start = at;
    if (c === '"') quoted = true;
  }
  push(text.length);
  return terms;
}

function readTerm(text: string, start: number, end: number): QueryTerm {
  const raw = text.slice(start, end);
  const negated = raw.startsWith("-") && raw.length > 1;
  const body = negated ? raw.slice(1) : raw;
  const base = { start, end, raw, negated, body };
  if (body.startsWith('"')) return { ...base, kind: "text", field: null, operand: body };
  if (body.startsWith("@")) {
    const colon = body.indexOf(":");
    return colon < 0
      ? { ...base, kind: "label", field: body.slice(1), operand: "" }
      : { ...base, kind: "label", field: body.slice(1, colon), operand: body.slice(colon + 1) };
  }
  const quote = body.indexOf('"');
  const colon = body.indexOf(":");
  if (colon > 0 && (quote < 0 || colon < quote)) {
    return { ...base, kind: "field", field: body.slice(0, colon), operand: body.slice(colon + 1) };
  }
  return { ...base, kind: "text", field: null, operand: body };
}

/** Whether a term names a field of the language. */
export function isKnownField(term: QueryTerm): boolean {
  return term.kind === "field" && term.field !== null && FIELD_NAMES.has(term.field);
}

/** The values of an operand, split on commas outside quotes, unquoted. */
export function operandValues(operand: string): string[] {
  const values: string[] = [];
  let current = "";
  let quoted = false;
  let escaped = false;
  for (const c of operand) {
    if (quoted) {
      if (escaped) {
        current += c;
        escaped = false;
      } else if (c === "\\") escaped = true;
      else if (c === '"') quoted = false;
      else current += c;
      continue;
    }
    if (c === '"') quoted = true;
    else if (c === ",") {
      values.push(current);
      current = "";
    } else current += c;
  }
  values.push(current);
  return values;
}

/** A value as a query writes it: quoted when it holds a space, a comma, a quote or a `*`. */
export function quoteValue(value: string): string {
  if (value !== "" && !/[\s,"*\\]/.test(value)) return value;
  return `"${value.replace(/[\\"]/g, (c) => `\\${c}`)}"`;
}

/** A label's key and value as a term: `@agent:triage`. */
export function labelTerm(key: string, value: string): string {
  return `@${key}:${quoteValue(value)}`;
}

/** `field:value`, the value quoted when it must be. `label:<key>` makes a label's term. */
export function fieldTerm(field: string, value: string): string {
  if (field.startsWith("@")) return labelTerm(field.slice(1), value);
  return `${field}:${quoteValue(value)}`;
}

/** Joins parts of a query, the empty ones left out. */
export function joinQuery(...parts: (string | null | undefined | false)[]): string {
  return parts
    .filter((p): p is string => typeof p === "string")
    .map((p) => p.trim())
    .filter((p) => p !== "")
    .join(" ");
}

/** The query with a term added, unless the query already has it. */
export function withTerm(text: string, term: string): string {
  if (splitTerms(text).some((t) => t.raw === term)) return text.trim();
  return joinQuery(text, term);
}

function splice(text: string, start: number, end: number, insert: string): string {
  return joinQuery(text.slice(0, start), insert, text.slice(end)).replace(/\s{2,}/g, " ");
}

/** A facet: a field (`status`, `tool`…) or a label's key written `@agent`. */
function matches(term: QueryTerm, field: string): boolean {
  if (field.startsWith("@")) return term.kind === "label" && term.field === field.slice(1);
  return term.kind === "field" && term.field === field;
}

export type FacetState = "include" | "exclude" | null;

/** Whether the query asks for a facet's value, or excludes it. */
export function facetState(text: string, field: string, value: string): FacetState {
  for (const term of splitTerms(text)) {
    if (!matches(term, field)) continue;
    if (operandValues(term.operand).includes(value)) return term.negated ? "exclude" : "include";
  }
  return null;
}

/** The values of a facet the query names, asked for or excluded. */
export function facetValues(text: string, field: string): { value: string; state: FacetState }[] {
  const out: { value: string; state: FacetState }[] = [];
  for (const term of splitTerms(text)) {
    if (!matches(term, field)) continue;
    for (const value of operandValues(term.operand))
      if (value !== "") out.push({ value, state: term.negated ? "exclude" : "include" });
  }
  return out;
}

/** The query without one value of a facet; a term left without a value goes. */
function removeValue(text: string, field: string, value: string, negated: boolean): string {
  for (const term of splitTerms(text)) {
    if (!matches(term, field) || term.negated !== negated) continue;
    const values = operandValues(term.operand);
    if (!values.includes(value)) continue;
    const rest = values.filter((v) => v !== value);
    const head = term.raw.slice(0, term.raw.length - term.operand.length);
    return splice(
      text,
      term.start,
      term.end,
      rest.length === 0 ? "" : `${head}${rest.map(quoteValue).join(",")}`,
    );
  }
  return text;
}

/** A click on a facet's value: asked for, then excluded, then gone. */
export function toggleFacet(text: string, field: string, value: string): string {
  const state = facetState(text, field, value);
  if (state === null) return joinQuery(text, fieldTerm(field, value));
  if (state === "include")
    return joinQuery(removeValue(text, field, value, false), `-${fieldTerm(field, value)}`);
  return removeValue(text, field, value, true).trim();
}

/** A term removed from the query, by its position. */
export function removeTerm(text: string, term: QueryTerm): string {
  return splice(text, term.start, term.end, "");
}

/** UTF-16 offsets of a span the server gives in characters (Unicode scalar values). */
export function utf16Span(text: string, start: number, end: number): [number, number] {
  const offsets: number[] = [];
  let at = 0;
  for (const c of text) {
    offsets.push(at);
    at += c.length;
  }
  offsets.push(at);
  const pick = (n: number) => offsets[Math.max(0, Math.min(n, offsets.length - 1))] ?? at;
  return [pick(start), pick(Math.max(start, end))];
}

export interface Suggestion {
  /** The term the suggestion writes in place of the one being typed. */
  insert: string;
  label: string;
  detail?: string;
  /** Runs with this value, from the facets. */
  count?: number;
  /** A field or a key: values follow, so the suggestions stay open. */
  partial?: boolean;
  /** Written as is: a pattern or a comparison, never quoted. */
  raw?: boolean;
}

export interface Suggestions {
  /** The part of the query the suggestion replaces. */
  from: number;
  to: number;
  items: Suggestion[];
}

const TIME_VALUES = [">now-1h", ">now-24h", ">now-7d", ">now-30d"];
const DURATION_VALUES = [">1s", ">1m", ">1h", "<1s"];
const COUNT_VALUES = [">0", ">=10", "0"];

function startsWith(value: string, typed: string): boolean {
  return value.toLowerCase().startsWith(typed.toLowerCase());
}

function counted(values: { value: string; runs: number }[] | undefined): Map<string, number> {
  return new Map((values ?? []).map((v) => [v.value, v.runs]));
}

/** The values a field takes, most frequent first where the facets count them. */
function fieldValues(field: string, facets: Facets | undefined): Suggestion[] {
  const from = (values: { value: string; runs: number }[] | undefined) =>
    (values ?? []).map((v) => ({ insert: v.value, label: v.value, count: v.runs }));
  switch (field) {
    case "status": {
      const counts = counted(facets?.statuses);
      return RUN_STATUSES.map((s: RunStatus) => ({
        insert: s,
        label: s,
        ...(counts.has(s) ? { count: counts.get(s) } : {}),
      }));
    }
    case "waiting": {
      const counts = counted(facets?.waiting);
      return [
        { insert: "*", label: "*", detail: "waiting for anything", raw: true },
        ...WAIT_KINDS.map((k: WaitKind) => ({
          insert: k,
          label: k,
          ...(counts.has(k) ? { count: counts.get(k) } : {}),
        })),
      ];
    }
    case "tool": {
      const tools = from(facets?.tools);
      const namespaces = [
        ...new Set(
          (facets?.tools ?? []).flatMap((t) =>
            t.value.includes(".") ? [`${t.value.slice(0, t.value.indexOf("."))}.*`] : [],
          ),
        ),
      ].map((ns) => ({ insert: ns, label: ns, detail: "every tool of the namespace", raw: true }));
      return [...tools, ...namespaces];
    }
    case "error":
      return from(facets?.errors);
    case "signal":
      return from(facets?.signals);
    case "script":
      return (facets?.scripts ?? []).map((v) => ({
        insert: v.value,
        label: v.value.slice(0, 12),
        count: v.runs,
      }));
    case "quarantined":
      return [
        { insert: "yes", label: "yes" },
        { insert: "no", label: "no" },
      ];
    case "mode":
      return [
        { insert: "live", label: "live" },
        { insert: "mock", label: "mock" },
      ];
    case "started":
    case "ended":
    case "activity":
      return TIME_VALUES.map((v) => ({ insert: v, label: v, raw: true }));
    case "duration":
      return DURATION_VALUES.map((v) => ({ insert: v, label: v, raw: true }));
    case "calls":
    case "errors":
      return COUNT_VALUES.map((v) => ({ insert: v, label: v, raw: true }));
    default:
      return [];
  }
}

/** The values typed before the last comma of an operand, kept as written. */
function before(operand: string): { kept: string; typed: string } {
  const values = operandValues(operand);
  const typed = values.pop() ?? "";
  return { kept: values.map(quoteValue).join(","), typed };
}

/**
 * What to suggest for the term the caret is in: field names and label keys while a name is
 * typed, then the values that exist for it (the facets of the current query), at most `limit`.
 */
export function suggest(
  text: string,
  caret: number,
  facets: Facets | undefined,
  limit = 12,
): Suggestions | null {
  const term = splitTerms(text).find((t) => t.start <= caret && caret <= t.end);
  const from = term ? term.start + (term.negated || term.raw === "-" ? 1 : 0) : caret;
  const to = term ? term.end : caret;
  const typed = text.slice(from, Math.max(from, caret));
  if (typed.startsWith('"')) return null;
  let items: Suggestion[];
  const colon = typed.indexOf(":");
  if (typed.startsWith("@") && colon > 0) {
    const key = typed.slice(1, colon);
    const { kept, typed: value } = before(typed.slice(colon + 1));
    const label = facets?.labels.find((l) => l.key === key);
    items = (label?.values ?? [])
      .filter((v) => startsWith(v.value, value))
      .map((v) => ({
        insert: `@${key}:${kept ? `${kept},` : ""}${quoteValue(v.value)}`,
        label: `@${key}:${v.value}`,
        count: v.runs,
      }));
  } else if (colon > 0 && FIELD_NAMES.has(typed.slice(0, colon))) {
    const field = typed.slice(0, colon);
    const { kept, typed: value } = before(typed.slice(colon + 1));
    items = fieldValues(field, facets)
      .filter((s) => startsWith(s.insert, value) || startsWith(s.label, value))
      .map((s) => ({
        ...s,
        insert: `${field}:${kept ? `${kept},` : ""}${s.raw ? s.insert : quoteValue(s.insert)}`,
        label: `${field}:${s.label}`,
      }));
  } else if (colon < 0) {
    const name = typed.startsWith("@") ? typed.slice(1) : typed;
    const keys = (facets?.labels ?? [])
      .filter((l) => startsWith(l.key, name))
      .map((l) => ({
        insert: `@${l.key}:`,
        label: `@${l.key}:`,
        detail: "label",
        count: l.runs,
        partial: true,
      }));
    const fields = typed.startsWith("@")
      ? []
      : FIELDS.filter((f) => startsWith(f.name, name)).map((f) => ({
          insert: `${f.name}:`,
          label: `${f.name}:`,
          detail: f.doc,
          partial: true,
        }));
    items = [...fields, ...keys];
  } else {
    return null;
  }
  // Nothing to add to what is typed in full.
  items = items.filter((s) => s.insert !== text.slice(from, to));
  if (items.length === 0) return null;
  return { from, to, items: items.slice(0, limit) };
}

/** The filter parameters of the Runs view before the query bar, by name. */
const LEGACY = [
  "status",
  "label",
  "tool",
  "script",
  "error",
  "quarantined",
  "parent",
  "range",
  "after",
  "before",
];

const RANGE_TERMS: Record<string, string> = {
  "1h": "started:>now-1h",
  "24h": "started:>now-24h",
  "7d": "started:>now-7d",
  "30d": "started:>now-30d",
};

/**
 * The query an older link means (`?status=failed&label=agent:triage&tool=…`), joined to its `q`;
 * `null` when the link has none of those parameters.
 */
export function legacyQuery(params: URLSearchParams): string | null {
  if (!LEGACY.some((name) => params.has(name))) return null;
  const terms: string[] = [];
  const one = (name: string) => params.get(name)?.trim() || null;
  const status = one("status");
  if (status && RUN_STATUSES.includes(status as RunStatus)) terms.push(`status:${status}`);
  for (const pair of params.getAll("label")) {
    const at = pair.indexOf(":");
    if (at > 0) terms.push(labelTerm(pair.slice(0, at), pair.slice(at + 1)));
  }
  for (const name of ["tool", "script", "error", "parent"]) {
    const value = one(name);
    if (value) terms.push(fieldTerm(name, value));
  }
  const quarantined = one("quarantined");
  if (quarantined === "yes" || quarantined === "no") terms.push(`quarantined:${quarantined}`);
  const range = one("range");
  if (range && Object.hasOwn(RANGE_TERMS, range)) terms.push(RANGE_TERMS[range] as string);
  else {
    const after = one("after");
    const before = one("before");
    if (after && !Number.isNaN(Date.parse(after))) terms.push(`started:>=${after}`);
    if (before && !Number.isNaN(Date.parse(before))) terms.push(`started:<${before}`);
  }
  return joinQuery(params.get("q"), ...terms);
}

/** A link's search with the query in `q`, older filter parameters dropped. */
export function withQuery(params: URLSearchParams, q: string): URLSearchParams {
  const next = new URLSearchParams(params);
  for (const name of LEGACY) next.delete(name);
  if (q.trim()) next.set("q", q.trim());
  else next.delete("q");
  return next;
}

/** `/runs?q=…`, and the same for the other views that take a query. */
export function queryLink(path: string, q: string, extra: Record<string, string> = {}): string {
  const params = new URLSearchParams();
  if (q.trim()) params.set("q", q.trim());
  for (const [k, v] of Object.entries(extra)) params.set(k, v);
  const search = params.toString();
  return search ? `${path}?${search}` : path;
}

/** An RFC 3339 time a query reads, without milliseconds. */
export function queryTime(ms: number): string {
  return new Date(ms).toISOString().replace(/\.\d{3}Z$/, "Z");
}
