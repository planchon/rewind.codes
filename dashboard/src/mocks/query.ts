// The query language, read as the server reads it (crates/rewind-api/src/query.rs), for the mocked
// API: a query becomes a predicate over the mocked run summaries, or the error the server would
// answer with `invalid-query`, with the position of the term at fault in characters.

import { FIELDS, operandValues, splitTerms } from "../model/query";
import { namespaceNameProblem } from "../model/quotas";
import type { RunMode, RunStatus, RunSummary, WaitKind } from "../model/types";
import { RUN_STATUSES, WAIT_KINDS } from "../model/types";

/** A run summary with what the read model's other tables know of it. */
export interface MockRun extends RunSummary {
  tools: string[];
  failedKinds: string[];
  mode: RunMode;
}

export class QueryError extends Error {
  constructor(
    message: string,
    readonly start: number,
    readonly end: number,
  ) {
    super(message);
  }
}

/**
 * The namespace the `q` of a statistics route names, `null` for the tenant (archi/05,
 * "Statistics"): one `namespace:` term of one value, or nothing; any other term is refused with its
 * position, as the server refuses it.
 */
export function scopeOf(q: string | null | undefined): string | null {
  const text = q ?? "";
  const terms = splitTerms(text);
  const chars = (at: number) => [...text.slice(0, at)].length;
  for (const term of terms) {
    const values = operandValues(term.operand);
    const value = values[0] ?? "";
    const scope =
      term.kind === "field" &&
      term.field === "namespace" &&
      !term.negated &&
      values.length === 1 &&
      value !== "" &&
      !value.includes("*");
    if (!scope || terms.length > 1)
      throw new QueryError(
        `the statistics are counted per namespace and hour: ${term.raw} cannot narrow them; name one namespace (namespace:alice) or none`,
        chars(term.start),
        chars(term.end),
      );
    return value;
  }
  return null;
}

export type Predicate = (run: MockRun) => boolean;

const UNITS: Record<string, number> = {
  w: 604_800_000,
  d: 86_400_000,
  h: 3_600_000,
  m: 60_000,
  s: 1_000,
  ms: 1,
};

/** `500ms`, `30s`, `1h30m`, `2d`, or a number of milliseconds; `null` otherwise. */
export function parseDuration(text: string): number | null {
  if (/^\d+$/.test(text)) return Number(text);
  const parts = [...text.matchAll(/(\d+)(ms|w|d|h|m|s)/g)];
  if (parts.length === 0 || parts.map((p) => p[0]).join("") !== text) return null;
  return parts.reduce((ms, p) => ms + Number(p[1]) * (UNITS[p[2] ?? ""] ?? 0), 0);
}

interface Range {
  /** Inclusive and exclusive bounds; `null` for none. */
  min: number | null;
  minIncluded: boolean;
  max: number | null;
  maxIncluded: boolean;
}

function within(value: number, r: Range): boolean {
  if (r.min !== null && (r.minIncluded ? value < r.min : value <= r.min)) return false;
  if (r.max !== null && (r.maxIncluded ? value > r.max : value >= r.max)) return false;
  return true;
}

type Alone = "exact" | "day" | "refused";

function comparison(
  name: string,
  operand: string,
  value: (text: string) => { v: number; next?: number },
  alone: Alone,
): Range {
  const op = /^(>=|<=|>|<)/.exec(operand)?.[1];
  const bad = (e: unknown) => new Error(`${name}: ${(e as Error).message}`);
  const read = (text: string) => {
    try {
      return value(text);
    } catch (e) {
      throw bad(e);
    }
  };
  if (op) {
    const { v } = read(operand.slice(op.length));
    if (op === ">") return { min: v, minIncluded: false, max: null, maxIncluded: false };
    if (op === ">=") return { min: v, minIncluded: true, max: null, maxIncluded: false };
    if (op === "<") return { min: null, minIncluded: false, max: v, maxIncluded: false };
    return { min: null, minIncluded: false, max: v, maxIncluded: true };
  }
  const dots = operand.indexOf("..");
  if (dots >= 0) {
    const from = operand.slice(0, dots);
    const to = operand.slice(dots + 2);
    if (!from && !to) throw new Error(`${name}: a range needs a start or an end, as a..b`);
    return {
      min: from ? read(from).v : null,
      minIncluded: true,
      max: to ? read(to).v : null,
      maxIncluded: false,
    };
  }
  const { v, next } = read(operand);
  if (alone === "day" && next !== undefined)
    return { min: v, minIncluded: true, max: next, maxIncluded: false };
  if (alone === "exact") return { min: v, minIncluded: true, max: v, maxIncluded: true };
  if (alone === "day")
    throw new Error(
      `${name}: compare a time with >, >=, <, <= or a range a..b, as ${name}:>now-24h, or give a date alone`,
    );
  throw new Error(`${name}: compare with >, >=, <, <= or a range a..b, as ${name}:>5m`);
}

function timeValue(now: number) {
  return (text: string): { v: number; next?: number } => {
    if (text === "now") return { v: now };
    if (text.startsWith("now-")) {
      const ago = parseDuration(text.slice(4));
      if (ago === null) throw new Error(`"${text.slice(4)}" is not a duration, as 24h, 30m or 7d`);
      return { v: now - ago };
    }
    if (/^\d{4}-\d{2}-\d{2}$/.test(text)) {
      const day = Date.parse(`${text}T00:00:00Z`);
      if (!Number.isNaN(day)) return { v: day, next: day + 86_400_000 };
    }
    const at = /^\d{4}-\d{2}-\d{2}T/.test(text) ? Date.parse(text) : Number.NaN;
    if (Number.isNaN(at))
      throw new Error(
        `"${text}" is not a time: write a date (2026-11-01), an RFC 3339 time (2026-11-01T10:00:00Z) or now-24h`,
      );
    return { v: at };
  };
}

interface Pattern {
  any?: boolean;
  exact?: string;
  prefix?: string;
}

function splitValues(operand: string): { value: string; quoted: boolean }[] {
  // Which values were quoted: a value is quoted when its text starts with a quote.
  const raw: string[] = [];
  let current = "";
  let quoted = false;
  let escaped = false;
  for (const c of operand) {
    if (quoted) {
      if (escaped) escaped = false;
      else if (c === "\\") escaped = true;
      else if (c === '"') quoted = false;
      current += c;
      continue;
    }
    if (c === '"') quoted = true;
    if (c === ",") {
      raw.push(current);
      current = "";
    } else current += c;
  }
  if (quoted) throw new Error('a quote is not closed: end the text with "');
  raw.push(current);
  const values = operandValues(operand);
  return raw.map((r, i) => {
    if (r.includes('"') && !(r.startsWith('"') && r.endsWith('"')))
      throw new Error('a quote inside a value: quote the whole value, as tool:"a b"');
    const value = values[i] ?? "";
    if (value === "" && !r.startsWith('"'))
      throw new Error("an empty value: write a value between the commas");
    return { value, quoted: r.startsWith('"') };
  });
}

function patterns(operand: string): Pattern[] {
  if (!operand) throw new Error("expected a value after the colon, or * for any value");
  return splitValues(operand).map(({ value, quoted }) => {
    if (quoted) return { exact: value };
    if (value === "*") return { any: true };
    if (value.endsWith("*")) {
      const prefix = value.slice(0, -1);
      if (prefix.includes("*")) throw new Error(`"${value}": * only ends a value, as github.*`);
      return { prefix };
    }
    if (value.includes("*")) throw new Error(`"${value}": * only ends a value, as github.*`);
    return { exact: value };
  });
}

function matchPattern(value: string, p: Pattern): boolean {
  if (p.any) return true;
  if (p.prefix !== undefined) return value.startsWith(p.prefix);
  return value === p.exact;
}

function exact(name: string, operand: string): string[] {
  return splitValues(operand).map(({ value, quoted }) => {
    if (!quoted && value.includes("*")) throw new Error(`${name}: * is not accepted here`);
    return value;
  });
}

function enumerated<T extends string>(name: string, operand: string, allowed: readonly T[]): T[] {
  return exact(name, operand).map((value) => {
    if (!allowed.includes(value as T))
      throw new Error(`${name}: "${value}" is not one of ${allowed.join(", ")}`);
    return value as T;
  });
}

function hexPrefixes(name: string, operand: string, hyphens: boolean): string[] {
  return exact(name, operand).map((value) => {
    const prefix = (value.endsWith("*") ? value.slice(0, -1) : value).toLowerCase();
    const digits = [...prefix].filter((c) => /[0-9a-f]/.test(c)).length;
    const valid = [...prefix].every((c) => /[0-9a-f]/.test(c) || (hyphens && c === "-"));
    if (!valid || digits < 8)
      throw new Error(`${name}: "${value}" is not a prefix of at least 8 hexadecimal digits`);
    return prefix.replaceAll("-", "");
  });
}

const QUOTED_TEXT = /^"((?:[^"\\]|\\.)*)"(.*)$/s;

function field(name: string, operand: string, now: number): Predicate {
  if (!operand) throw new Error(`${name}: expected a value after the colon`);
  const ended = (r: MockRun) => (r.endedAt ? Date.parse(r.endedAt) : null);
  switch (name) {
    case "namespace": {
      const names = exact(name, operand).map((value) => {
        if (namespaceNameProblem(value) !== null)
          throw new Error(
            `namespace: "${value}" is not a namespace: 1 to 128 letters, digits, -, _, ., : and @`,
          );
        return value;
      });
      return (r) => names.includes(r.namespace);
    }
    case "status": {
      const statuses = enumerated<RunStatus>(name, operand, RUN_STATUSES);
      return (r) => statuses.includes(r.status);
    }
    case "tool": {
      const ps = patterns(operand);
      return (r) => r.tools.some((t) => ps.some((p) => matchPattern(t, p)));
    }
    case "signal": {
      const ps = patterns(operand);
      return (r) =>
        (r.waitingFor ?? []).some(
          (w) => w.kind === "signal" && ps.some((p) => matchPattern(w.name ?? "", p)),
        );
    }
    case "error": {
      const errors = exact(name, operand);
      return (r) =>
        errors.some((e) => r.error === e || r.failedKinds.includes(e as (typeof r.failedKinds)[0]));
    }
    case "waiting": {
      const kinds = operand === "*" ? [] : enumerated<WaitKind>(name, operand, WAIT_KINDS);
      return (r) =>
        r.status === "active" &&
        (r.waitingFor ?? []).length > 0 &&
        (kinds.length === 0 || (r.waitingFor ?? []).some((w) => kinds.includes(w.kind)));
    }
    case "started":
    case "ended":
    case "activity": {
      const range = comparison(name, operand, timeValue(now), "day");
      return (r) => {
        const at =
          name === "started"
            ? Date.parse(r.startedAt)
            : name === "ended"
              ? ended(r)
              : Date.parse(r.lastActivityAt);
        return at !== null && within(at, range);
      };
    }
    case "duration": {
      const range = comparison(
        name,
        operand,
        (text) => {
          const ms = parseDuration(text);
          if (ms === null)
            throw new Error(`"${text}" is not a duration, as 500ms, 30s, 5m, 1h30m or 2d`);
          return { v: ms };
        },
        "refused",
      );
      return (r) => {
        const end = ended(r);
        return end !== null && within(end - Date.parse(r.startedAt), range);
      };
    }
    case "calls":
    case "errors": {
      const range = comparison(
        name,
        operand,
        (text) => {
          if (!/^\d+$/.test(text)) throw new Error(`"${text}" is not a number`);
          return { v: Number(text) };
        },
        "exact",
      );
      return (r) => within(name === "calls" ? r.toolCalls : r.errors, range);
    }
    case "script":
    case "toolset": {
      const prefixes = hexPrefixes(name, operand, false);
      return (r) => prefixes.some((p) => (name === "script" ? r.script : r.toolset).startsWith(p));
    }
    case "id": {
      const prefixes = hexPrefixes(name, operand, true);
      return (r) => prefixes.some((p) => r.id.replaceAll("-", "").startsWith(p));
    }
    case "parent":
    case "started_by": {
      const id = operand.toLowerCase();
      const digits = id.replaceAll("-", "");
      if (digits.length !== 32 || !/^[0-9a-f]+$/.test(digits))
        throw new Error(`${name}: "${operand}" is not a run id`);
      return (r) => (name === "parent" ? r.parent : r.startedBy)?.replaceAll("-", "") === digits;
    }
    case "workflow": {
      const names = exact(name, operand);
      return (r) => r.workflow != null && names.includes(r.workflow);
    }
    case "fingerprint": {
      const values = exact(name, operand).map((v) => {
        if (!/^[0-9a-fA-F]{16}$/.test(v))
          throw new Error(`fingerprint: "${v}" is not a fingerprint: 16 hexadecimal digits`);
        return v.toLowerCase();
      });
      return (r) => r.fingerprint != null && values.includes(r.fingerprint);
    }
    case "quarantined": {
      const yes = operand === "yes" || operand === "true";
      if (!yes && operand !== "no" && operand !== "false")
        throw new Error(`quarantined: expected yes or no, not "${operand}"`);
      return (r) => r.quarantined === yes;
    }
    case "mode": {
      const modes = enumerated<RunMode>(name, operand, ["live", "mock"]);
      return (r) => modes.includes(r.mode);
    }
    default:
      throw new Error(`unknown field "${name}"`);
  }
}

function textPredicate(text: string): Predicate {
  const needle = text.toLowerCase();
  return (r) =>
    [r.errorMessage ?? "", r.headline, r.id, ...Object.values(r.labels)].some((h) =>
      h.toLowerCase().includes(needle),
    );
}

function term(body: string, now: number): Predicate {
  if (body.startsWith('"')) {
    const m = QUOTED_TEXT.exec(body);
    if (!m) throw new Error('a quote is not closed: end the text with "');
    if (m[2])
      throw new Error(`unexpected "${m[2]}" after the quoted text: separate terms with a space`);
    const text = (m[1] ?? "").replace(/\\(["\\])/g, "$1");
    if (!text.trim()) throw new Error("empty text: write something between the quotes");
    return textPredicate(text);
  }
  if (body.startsWith("@")) {
    const label = body.slice(1);
    const colon = label.indexOf(":");
    if (colon < 0)
      throw new Error(`a label is written @key:value, @key:prefix* or @key:*, not @${label}`);
    const key = label.slice(0, colon);
    if (!key || key.includes('"')) throw new Error("a label needs a key: @key:value");
    const ps = patterns(label.slice(colon + 1));
    return (r) => {
      const value = r.labels[key];
      return value !== undefined && ps.some((p) => matchPattern(value, p));
    };
  }
  const quote = body.indexOf('"');
  const colon = body.slice(0, quote < 0 ? body.length : quote).indexOf(":");
  if (colon < 0) {
    if (quote >= 0) throw new Error('a quote inside a word: quote the whole text, as "rate limit"');
    return textPredicate(body);
  }
  const name = body.slice(0, colon);
  if (!/^[a-z]+$/.test(name) || !FIELDS.some((f) => f.name === name)) {
    throw new Error(
      `unknown field "${name}": the fields are ${FIELDS.map((f) => f.name).join(", ")}; a label is written @key:value; quote text that contains a colon`,
    );
  }
  return field(name, body.slice(colon + 1), now);
}

/** Character offsets of each UTF-16 offset. */
function charIndex(text: string, utf16: number): number {
  return [...text.slice(0, utf16)].length;
}

/** Reads a query into a predicate, or throws the error the server answers with. */
export function compileQuery(text: string | undefined, now: number): Predicate {
  if (!text) return () => true;
  const predicates: Predicate[] = [];
  let start = -1;
  let quote = -1;
  let escaped = false;
  const spans: [number, number][] = [];
  for (let at = 0; at < text.length; at += 1) {
    const c = text[at] ?? "";
    if (quote >= 0) {
      if (escaped) escaped = false;
      else if (c === "\\") escaped = true;
      else if (c === '"') quote = -1;
      continue;
    }
    if (/\s/.test(c)) {
      if (start >= 0) spans.push([start, at]);
      start = -1;
      continue;
    }
    if (start < 0) start = at;
    if (c === '"') quote = at;
  }
  if (quote >= 0)
    throw new QueryError(
      'a quote is not closed: end the text with "',
      charIndex(text, quote),
      charIndex(text, text.length),
    );
  if (start >= 0) spans.push([start, text.length]);
  for (const [from, to] of spans) {
    const raw = text.slice(from, to);
    try {
      if (raw === "-")
        throw new Error(
          'a lone "-": write the term to exclude right after it, as -status:completed',
        );
      const negated = raw.startsWith("-");
      const p = term(negated ? raw.slice(1) : raw, now);
      predicates.push(negated ? (r) => !p(r) : p);
    } catch (e) {
      throw new QueryError((e as Error).message, charIndex(text, from), charIndex(text, to));
    }
  }
  return (r) => predicates.every((p) => p(r));
}
