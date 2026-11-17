// The searches of Tools, Scripts and Namespaces: one line of text in the query bar, as on the
// Overview, read here into what each view asks the API for or keeps of what it lists. The runs'
// query language gives the syntax (terms, `field:value`, `@key:value`, `-` to exclude, quotes);
// each view takes the fields it can answer, and says which term it cannot and why, as the server
// does for the runs' query.
import {
  type FieldSpec,
  fieldTerm,
  joinQuery,
  labelTerm,
  operandValues,
  type QueryTerm,
  splitTerms,
} from "./query";

/** A term a view cannot read: where it is, in characters as the server counts them, and why. */
export interface SearchFault {
  message: string;
  start: number;
  end: number;
}

function faultAt(text: string, term: QueryTerm, message: string): SearchFault {
  return {
    message,
    start: [...text.slice(0, term.start)].length,
    end: [...text.slice(0, term.end)].length,
  };
}

/** A text term without its quotes. */
function unquoted(body: string): string {
  return operandValues(body).join(",");
}

// Tools

export const TOOL_FIELDS: readonly FieldSpec[] = [
  { name: "tool", doc: "tool:github.close_issue, every tool of a namespace: tool:github.*" },
  { name: "toolset", doc: "the calls of one toolset: its digest" },
];

export interface ToolSearch {
  /** The toolset the API reads the calls of. */
  toolset: string | null;
  /** Whether a tool is listed. */
  keeps: (tool: string) => boolean;
  fault: SearchFault | null;
}

const TOOL_HINT = "search the tools by tool:, toolset: or a part of their name";

function toolPattern(pattern: string): (tool: string) => boolean {
  if (pattern.endsWith("*")) {
    const head = pattern.slice(0, -1);
    return (tool) => tool.startsWith(head);
  }
  return (tool) => tool === pattern;
}

/**
 * The Tools view's search: `toolset:<digest>` for the API; `tool:` (a name, or `github.*`) and
 * words of a tool's name for the list, each term required, `-` excluding.
 */
export function readToolSearch(q: string): ToolSearch {
  const tests: ((tool: string) => boolean)[] = [];
  let toolset: string | null = null;
  const refuse = (term: QueryTerm, message: string): ToolSearch => ({
    toolset: null,
    keeps: () => true,
    fault: faultAt(q, term, message),
  });
  for (const term of splitTerms(q)) {
    let test: (tool: string) => boolean;
    if (term.kind === "text") {
      const word = unquoted(term.body).toLowerCase();
      test = (tool) => tool.toLowerCase().includes(word);
    } else if (term.kind === "field" && term.field === "tool") {
      const patterns = operandValues(term.operand).filter((p) => p !== "");
      if (patterns.length === 0) return refuse(term, "tool: a tool's name, or github.*");
      const tests = patterns.map(toolPattern);
      test = (tool) => tests.some((matches) => matches(tool));
    } else if (term.kind === "field" && term.field === "toolset") {
      if (term.negated) return refuse(term, "toolset: the calls of one toolset, not excluded");
      if (toolset !== null) return refuse(term, "toolset: one toolset at a time");
      if (!/^[0-9a-f]{6,64}$/.test(term.operand))
        return refuse(term, "toolset: a toolset's digest, in hexadecimal");
      toolset = term.operand;
      continue;
    } else if (term.kind === "label") {
      return refuse(term, `A tool has no label: ${TOOL_HINT}`);
    } else {
      return refuse(term, `${term.field}: not a field of the tools; ${TOOL_HINT}`);
    }
    tests.push(term.negated ? (tool) => !test(tool) : test);
  }
  return { toolset, keeps: (tool) => tests.every((keeps) => keeps(tool)), fault: null };
}

// Scripts

export const SCRIPT_FIELDS: readonly FieldSpec[] = [
  { name: "tool", doc: "the scripts that called a tool: tool:github.close_issue" },
];

export interface ScriptSearch {
  /** The labels a script ran with, each `key:value`: all of them. */
  labels: string[];
  /** The tool a script called. */
  tool: string | null;
  fault: SearchFault | null;
}

const SCRIPT_HINT = "search the scripts by tool:github.close_issue and @key:value";

/** The Scripts view's search, all for the API: `tool:<name>` and labels, `@key:value`. */
export function readScriptSearch(q: string): ScriptSearch {
  const labels: string[] = [];
  let tool: string | null = null;
  const refuse = (term: QueryTerm, message: string): ScriptSearch => ({
    labels: [],
    tool: null,
    fault: faultAt(q, term, message),
  });
  for (const term of splitTerms(q)) {
    if (term.negated) return refuse(term, `A script search cannot exclude: ${SCRIPT_HINT}`);
    const values = operandValues(term.operand);
    if (term.kind === "label") {
      const [value] = values;
      if (values.length !== 1 || !value)
        return refuse(term, "@key:value: one value per label; repeat the term to require several");
      labels.push(`${term.field}:${value}`);
    } else if (term.kind === "field" && term.field === "tool") {
      const [name] = values;
      if (values.length !== 1 || !name || name.includes("*"))
        return refuse(term, "tool: one tool, by its full name");
      if (tool !== null) return refuse(term, "tool: one tool at a time");
      tool = name;
    } else if (term.kind === "field") {
      return refuse(term, `${term.field}: not a field of the scripts; ${SCRIPT_HINT}`);
    } else {
      return refuse(term, `Scripts are not searched by their text: ${SCRIPT_HINT}`);
    }
  }
  return { labels, tool, fault: null };
}

/** The Scripts search of an older link's parameters (`?label=k:v&tool=…`). */
export function scriptQueryOf(params: URLSearchParams): string | null {
  if (!params.has("label") && !params.has("tool")) return null;
  const terms = params
    .getAll("label")
    .filter((pair) => pair.indexOf(":") > 0)
    .map((pair) => labelTerm(pair.slice(0, pair.indexOf(":")), pair.slice(pair.indexOf(":") + 1)));
  const tool = params.get("tool")?.trim();
  if (tool) terms.push(fieldTerm("tool", tool));
  return joinQuery(params.get("q"), ...terms);
}

// Namespaces

export interface NamespaceSearch {
  /** The start of the names listed. */
  prefix: string;
  fault: SearchFault | null;
}

/** The Namespaces view's search: the start of a name, as typed. */
export function readNamespaceSearch(q: string): NamespaceSearch {
  const prefix = q.trim();
  const bad = /[^A-Za-z0-9\-_.:@]/.exec(prefix);
  if (!bad) return { prefix, fault: null };
  const lead = q.length - q.trimStart().length;
  const at = [...q.slice(0, lead + bad.index)].length;
  return {
    prefix: "",
    fault: {
      message: "A namespace is searched by the start of its name: letters, digits and - _ . : @",
      start: at,
      end: at + 1,
    },
  };
}
