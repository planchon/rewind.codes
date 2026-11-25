// The searches of the lists that are not of runs (Workflows, Actions, Namespaces): one query bar, as
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
import type { ActionStatsQuery, ListWorkflowsQuery } from "./types";

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

/** A value of `action:`: a name, or the actions of a group (`github.*`). */
function actionMatches(value: string, action: string): boolean {
  return value.endsWith("*") ? action.startsWith(value.slice(0, -1)) : action === value;
}

// Workflows: `GET /v1/workflows` over a window, narrowed to one namespace by `Rewind-Namespace`
// and its counts by `q`; the actions a workflow called are filtered in the browser.

export const WORKFLOWS_LANGUAGE: QueryLanguage = {
  fields: [
    { name: "namespace", doc: "one end user's workflows and the organization's: namespace:alice" },
    { name: "action", doc: "workflows that called an action: action:github.close_issue, github.*" },
  ],
  labels: false,
};

const WORKFLOWS_TERMS =
  "Workflows are searched by namespace (namespace:alice) and action (action:github.list_issues): their counts come from counters, which know no labels";

export interface WorkflowsSearch {
  /** The scope of the counts (`q`) and the window are added by the list. */
  query: Omit<ListWorkflowsQuery, "since" | "until" | "after">;
  /** The namespace the list is narrowed to, sent as `Rewind-Namespace`. */
  namespace: string | null;
  /** Whether a workflow that called these actions in the window is kept. */
  keeps: (actions: readonly string[]) => boolean;
  /** The query holds a term the browser applies (`action:`). */
  filtered: boolean;
  fault: QueryFault | null;
}

/**
 * The request of `GET /v1/workflows` a query means: one namespace as the header that narrows the
 * list to an end user's workflows and the organization's, and as the scope of the counts; the
 * actions (`action:github.close_issue`, `action:github.*`, excluded with `-`) are kept in the
 * browser, from the actions each workflow's runs called. The counts come from the hourly counters
 * (archi/05, "Statistics"), which know no labels.
 */
export function workflowsSearch(q: string, limit = 50): WorkflowsSearch {
  const query: WorkflowsSearch["query"] = { limit };
  let namespace: string | null = null;
  const tests: ((actions: readonly string[]) => boolean)[] = [];
  const done = (fault: QueryFault | null): WorkflowsSearch => ({
    query: namespace ? { ...query, q: `namespace:${quoteValue(namespace)}` } : query,
    namespace,
    keeps: (actions) => tests.every((test) => test(actions)),
    filtered: tests.length > 0,
    fault,
  });
  for (const term of splitTerms(q)) {
    const values = valuesOf(term);
    const fault = (message: string) => done(termFault(q, term, message));
    if (term.kind === "field" && term.field === "namespace") {
      const name = values.length === 1 ? (values[0] ?? "") : "";
      if (term.negated) return fault("A namespace cannot be excluded: remove the -");
      if (values.length !== 1 || name.includes("*"))
        return fault("namespace: names one end user: namespace:alice");
      const problem = namespaceNameProblem(name);
      if (problem) return fault(problem);
      if (namespace !== null) return fault("One namespace at a time");
      namespace = name;
    } else if (term.kind === "field" && term.field === "action") {
      if (values.length === 0) return fault("action: needs an action: action:github.close_issue");
      const called = (actions: readonly string[]) =>
        values.some((v) => actions.some((a) => actionMatches(v, a)));
      tests.push(term.negated ? (actions) => !called(actions) : called);
    } else {
      return fault(WORKFLOWS_TERMS);
    }
  }
  return done(null);
}

/**
 * `?label=agent:triage&tool=x`, as the Scripts view's links wrote it: a label is read as its term,
 * which the bar then underlines, since the list cannot filter by label; a tool is an action.
 */
export function legacyWorkflowsQuery(params: URLSearchParams): string | null {
  return legacy(params, (p) => [
    ...p
      .getAll("label")
      .filter((l) => l.indexOf(":") > 0)
      .map((l) => `@${l.slice(0, l.indexOf(":"))}:${quoteValue(l.slice(l.indexOf(":") + 1))}`),
    ...p.getAll("tool").map((t) => `action:${quoteValue(t)}`),
  ]);
}

// Actions: `GET /v1/actions/stats?q=namespace:…&group=…`, the actions filtered in the browser

export const ACTIONS_LANGUAGE: QueryLanguage = {
  fields: [
    { name: "action", doc: "these actions: action:github.close_issue, action:github.*" },
    { name: "namespace", doc: "the calls of one end user's runs: namespace:alice" },
  ],
  labels: false,
};

const ACTIONS_TERMS = "Actions are searched by action (action:github.*), namespace and name";

export interface ActionsSearch {
  /** The one namespace whose calls are counted; the organization's when `null`. */
  namespace: string | null;
  /** The group the server keeps: one `action:github.*` asked for. */
  group: string | null;
  /** Whether an action is kept. */
  keeps: (action: string) => boolean;
  fault: QueryFault | null;
}

/**
 * What a query keeps of the action statistics, kept per action by the hourly counters (archi/05,
 * "Statistics"): `namespace:` asks the server for one end user's calls, and one `action:github.*`
 * for the actions of the group `github`; `action:` (a name or a group), free text (in the action's
 * name) and their negations are applied to the actions the server returns.
 */
export function actionsSearch(q: string): ActionsSearch {
  let namespace: string | null = null;
  const patterns: string[] = [];
  const tests: ((action: string) => boolean)[] = [];
  const done = (fault: QueryFault | null): ActionsSearch => {
    const only = patterns.length === 1 ? (patterns[0] ?? "") : "";
    const group = /^([^.*]+)\.\*$/.exec(only)?.[1] ?? null;
    return {
      namespace,
      group,
      keeps: (action: string) => tests.every((test) => test(action)),
      fault,
    };
  };
  for (const term of splitTerms(q)) {
    const values = valuesOf(term);
    const fault = (message: string) => done(termFault(q, term, message));
    let test: (action: string) => boolean;
    if (term.kind === "field" && term.field === "namespace") {
      const name = values.length === 1 ? (values[0] ?? "") : "";
      if (term.negated) return fault("A namespace cannot be excluded: remove the -");
      if (values.length !== 1 || name.includes("*"))
        return fault("namespace: names one end user: namespace:alice");
      const problem = namespaceNameProblem(name);
      if (problem) return fault(problem);
      if (namespace !== null) return fault("One namespace at a time");
      namespace = name;
      continue;
    }
    if (term.kind === "field" && term.field === "action_set")
      return fault(
        "Action statistics are kept per action, not per action set: search by action and name",
      );
    if (term.kind === "field" && term.field === "action") {
      if (values.length === 0) return fault("action: needs an action: action:github.close_issue");
      test = (action) => values.some((v) => actionMatches(v, action));
      if (!term.negated) patterns.push(values.length === 1 ? (values[0] ?? "") : "");
    } else if (term.kind === "text") {
      const text = textOf(term).toLowerCase();
      test = (action) => action.toLowerCase().includes(text);
    } else {
      return fault(ACTIONS_TERMS);
    }
    tests.push(term.negated ? (action) => !test(action) : test);
  }
  return done(null);
}

/**
 * `?toolset=…` and `?q=tool:…`, as the Tools view's links wrote them: a toolset is an action set,
 * a term the view explains it cannot read, and a tool an action.
 */
export function legacyActionsQuery(params: URLSearchParams): string | null {
  return legacy(params, (p) => p.getAll("toolset").map((t) => `action_set:${quoteValue(t)}`));
}

/**
 * The statistics a search of the Actions asks for: every call since the first (the server reads 24
 * hours without `since`), of a scope, of a group.
 */
export function toActionStatsQuery(
  search: Pick<ActionsSearch, "namespace" | "group">,
): ActionStatsQuery {
  const query: ActionStatsQuery = { since: SINCE_THE_FIRST };
  if (search.namespace) query.q = `namespace:${quoteValue(search.namespace)}`;
  if (search.group) query.group = search.group;
  return query;
}

/** A `since` before every run: the statistics since the first. */
export const SINCE_THE_FIRST = "1970-01-01T00:00:00.000Z";

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
