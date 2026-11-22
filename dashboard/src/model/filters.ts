// The searches of the lists that are not of runs (Scripts, Tools, Namespaces): one query bar, as
// the runs have, reading the part of the query language the list's endpoint understands, which
// is turned here into the endpoint's parameters. A term the list cannot read is a fault, which
// the bar underlines, as the server's are. The query (`q`) and the window are kept in the URL, so
// a filtered view can be shared; the parameters of older links are read as the query they mean.

import {
  joinQuery,
  operandValues,
  type QueryFault,
  type QueryLanguage,
  type QueryTerm,
  quoteValue,
  splitTerms,
  termFault,
} from "./query";
import { namespaceNameProblem } from "./quotas";
import type { ListScriptsQuery, ToolStatsQuery } from "./types";

export const RANGES = {
  "1h": 3_600_000,
  "24h": 86_400_000,
  "7d": 7 * 86_400_000,
  "30d": 30 * 86_400_000,
} as const;

export type RangeKey = keyof typeof RANGES;

export function isRange(value: string | null): value is RangeKey {
  // Own keys only: `"toString" in RANGES` holds too, and its "range" is a function.
  return value !== null && Object.hasOwn(RANGES, value);
}

/** The values a term names, the empty ones (a trailing comma) left out. */
function valuesOf(term: QueryTerm): string[] {
  return operandValues(term.operand).filter((v) => v !== "");
}

/** Free text, unquoted. */
function textOf(term: QueryTerm): string {
  return term.body.startsWith('"') ? (operandValues(term.body)[0] ?? "") : term.body;
}

/** The query of a link written before the query bar, from its parameters; `null` without any. */
function legacy(params: URLSearchParams, terms: (params: URLSearchParams) => string[]) {
  const parts = terms(params);
  if (parts.length === 0) return null;
  return joinQuery(params.get("q") ?? "", ...parts);
}

// Scripts: `GET /v1/scripts?label=…&tool=…`, narrowed to one namespace by `Rewind-Namespace`

export const SCRIPTS_LANGUAGE: QueryLanguage = {
  fields: [
    { name: "namespace", doc: "the scripts one end user's runs ran: namespace:alice" },
    { name: "tool", doc: "scripts that called this tool: tool:github.close_issue" },
  ],
  labels: true,
};

const SCRIPTS_TERMS =
  "Scripts are searched by namespace (namespace:alice), label (@agent:triage) and tool (tool:github.list_issues)";

export interface ScriptsSearch {
  query: ListScriptsQuery;
  /** The namespace the list is narrowed to, sent as `Rewind-Namespace`. */
  namespace: string | null;
  fault: QueryFault | null;
}

/**
 * The request of `GET /v1/scripts` a query means: its labels and one tool as parameters, and one
 * namespace as the header that narrows every read to an end user's runs.
 */
export function scriptsSearch(q: string, limit = 50): ScriptsSearch {
  const query: ListScriptsQuery = { limit };
  const labels: string[] = [];
  let namespace: string | null = null;
  for (const term of splitTerms(q)) {
    const values = valuesOf(term);
    const fault = (message: string) => ({ query, namespace, fault: termFault(q, term, message) });
    if (term.negated) return fault("Scripts cannot be excluded: remove the -");
    if (term.kind === "field" && term.field === "namespace") {
      const name = values.length === 1 ? (values[0] ?? "") : "";
      if (values.length !== 1 || name.includes("*"))
        return fault("namespace: names one end user: namespace:alice");
      const problem = namespaceNameProblem(name);
      if (problem) return fault(problem);
      if (namespace !== null) return fault("One namespace at a time");
      namespace = name;
      continue;
    }
    if (term.kind === "label" && term.field) {
      if (values.length !== 1) return fault("One value per label: @agent:triage");
      labels.push(`${term.field}:${values[0]}`);
    } else if (term.kind === "field" && term.field === "tool") {
      if (values.length !== 1 || values[0]?.includes("*"))
        return fault("tool: names one tool: tool:github.list_issues");
      if (query.tool) return fault("One tool at a time");
      query.tool = values[0];
    } else {
      return fault(SCRIPTS_TERMS);
    }
  }
  if (labels.length > 0) query.label = labels;
  return { query, namespace, fault: null };
}

/** `?label=agent:triage&tool=x`, as the Scripts view's links wrote it. */
export function legacyScriptsQuery(params: URLSearchParams): string | null {
  return legacy(params, (p) => [
    ...p
      .getAll("label")
      .filter((l) => l.indexOf(":") > 0)
      .map((l) => `@${l.slice(0, l.indexOf(":"))}:${quoteValue(l.slice(l.indexOf(":") + 1))}`),
    ...p.getAll("tool").map((t) => `tool:${quoteValue(t)}`),
  ]);
}

// Tools: `GET /v1/tools/stats?toolset=…`, the tools filtered in the browser

export const TOOLS_LANGUAGE: QueryLanguage = {
  fields: [
    { name: "tool", doc: "these tools: tool:github.close_issue, tool:github.*" },
    { name: "toolset", doc: "the calls of the runs of one toolset: its digest" },
  ],
  labels: false,
};

export interface ToolsSearch {
  toolset: string | null;
  /** Whether a tool is kept. */
  keeps: (tool: string) => boolean;
  fault: QueryFault | null;
}

/** A value of `tool:`: a name, or the tools of a namespace (`github.*`). */
function toolMatches(value: string, tool: string): boolean {
  return value.endsWith("*") ? tool.startsWith(value.slice(0, -1)) : tool === value;
}

/**
 * What a query keeps of the tool statistics: `toolset:` asks the server for one toolset's calls;
 * `tool:` (a name or a pattern), free text (in the tool's name) and their negations are applied
 * to the tools the server returns, all of a tenant's.
 */
export function toolsSearch(q: string): ToolsSearch {
  let toolset: string | null = null;
  const tests: ((tool: string) => boolean)[] = [];
  const done = (fault: QueryFault | null) => ({
    toolset,
    keeps: (tool: string) => tests.every((test) => test(tool)),
    fault,
  });
  for (const term of splitTerms(q)) {
    const values = valuesOf(term);
    const fault = (message: string) => done(termFault(q, term, message));
    let test: (tool: string) => boolean;
    if (term.kind === "field" && term.field === "toolset") {
      if (term.negated) return fault("A toolset cannot be excluded: remove the -");
      if (values.length !== 1 || !/^[0-9a-f]{8,64}$/.test(values[0] ?? ""))
        return fault("toolset: takes a toolset's digest, 8 hexadecimal digits at least");
      if (toolset !== null) return fault("One toolset at a time");
      toolset = values[0] ?? null;
      continue;
    }
    if (term.kind === "field" && term.field === "tool") {
      if (values.length === 0) return fault("tool: needs a tool: tool:github.close_issue");
      test = (tool) => values.some((v) => toolMatches(v, tool));
    } else if (term.kind === "text") {
      const text = textOf(term).toLowerCase();
      test = (tool) => tool.toLowerCase().includes(text);
    } else {
      return fault("Tools are searched by tool (tool:github.*), toolset and name");
    }
    tests.push(term.negated ? (tool) => !test(tool) : test);
  }
  return done(null);
}

/** `?toolset=…`, as the Tools view's links wrote it. */
export function legacyToolsQuery(params: URLSearchParams): string | null {
  return legacy(params, (p) => p.getAll("toolset").map((t) => `toolset:${quoteValue(t)}`));
}

export function toToolStatsQuery(
  window: RangeKey,
  toolset: string | null,
  now: number,
): ToolStatsQuery {
  const query: ToolStatsQuery = {
    since: new Date(now - RANGES[window]).toISOString(),
    until: new Date(now).toISOString(),
  };
  if (toolset) query.toolset = toolset;
  return query;
}

// Namespaces: `GET /v1/namespaces?prefix=…`

export const NAMESPACES_LANGUAGE: QueryLanguage = { fields: [], labels: false };

/** The start of the namespaces' names a query asks for: one name, or its start. */
export function namespacesSearch(q: string): { prefix: string | null; fault: QueryFault | null } {
  const terms = splitTerms(q);
  for (const [i, term] of terms.entries()) {
    const fault = (message: string) => ({ prefix: null, fault: termFault(q, term, message) });
    if (term.kind !== "text" || term.negated)
      return fault("Namespaces are searched by the start of their name");
    if (i > 0) return fault("One name, or its start");
  }
  const first = terms[0];
  return { prefix: first ? textOf(first) || null : null, fault: null };
}

/** `?prefix=…`, as the Namespaces view's links wrote it. */
export function legacyNamespacesQuery(params: URLSearchParams): string | null {
  return legacy(params, (p) => p.getAll("prefix").filter(Boolean).map(quoteValue));
}
