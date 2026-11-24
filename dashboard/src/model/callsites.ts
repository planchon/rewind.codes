// What a callsite points at in the source, found with Lezer's syntax tree (archi/08-decisions.md,
// "Taken while designing the dashboard's debugger and search"). An entry records the position of
// its callee's last name (`list_issues` in `github.list_issues`): that name leads to its call
// expression, what a hover shows, and to the declaration that binds the call's awaited result
// (`const x = await call()`, one level of destructuring). The run's input is bound to the first
// parameter of the default export.
import { parser } from "@lezer/javascript";
import type { Callsite } from "./types";

const typescript = parser.configure({ dialect: "ts" });

type Tree = ReturnType<typeof typescript.parse>;
type SyntaxNode = Tree["topNode"];

export interface SourceIndex {
  text: string;
  tree: Tree;
  /** The offset where each line starts, line 1 first. */
  lineStarts: number[];
}

export function indexSource(source: string): SourceIndex {
  const text = source.replace(/\r\n?/g, "\n");
  const lineStarts = [0];
  for (let at = text.indexOf("\n"); at >= 0; at = text.indexOf("\n", at + 1))
    lineStarts.push(at + 1);
  return { text, tree: typescript.parse(text), lineStarts };
}

/** The line (from 1) of an offset. */
export function lineAt(index: SourceIndex, offset: number): number {
  let lo = 0;
  let hi = index.lineStarts.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if ((index.lineStarts[mid] ?? 0) <= offset) lo = mid;
    else hi = mid - 1;
  }
  return lo + 1;
}

function lineEnd(index: SourceIndex, line: number): number {
  const next = index.lineStarts[line];
  return next === undefined ? index.text.length : next - 1;
}

export interface Span {
  from: number;
  to: number;
}

export interface CallSpan extends Span {
  /** The line the call expression starts on. */
  line: number;
  /** The callee's last name: `list_issues`, `sleep`, `signal`. */
  name: string;
  /** Where that name is. */
  nameFrom: number;
  nameTo: number;
}

/** The node of a callee's last name: the property of a member, a variable, under type arguments. */
function calleeName(callee: SyntaxNode | null): SyntaxNode | null {
  if (!callee) return null;
  switch (callee.name) {
    case "VariableName":
      return callee;
    case "MemberExpression":
      return callee.lastChild?.name === "PropertyName" ? callee.lastChild : null;
    case "InstantiationExpression":
    case "ParenthesizedExpression":
      return calleeName(callee.firstChild);
    default:
      return null;
  }
}

function callSpan(index: SourceIndex, call: SyntaxNode): CallSpan | null {
  const name = calleeName(call.firstChild);
  if (!name) return null;
  return {
    from: call.from,
    to: call.to,
    line: lineAt(index, call.from),
    name: index.text.slice(name.from, name.to),
    nameFrom: name.from,
    nameTo: name.to,
  };
}

/** The call expressions that start on a line, in order. */
function callsOnLine(index: SourceIndex, line: number): SyntaxNode[] {
  const from = index.lineStarts[line - 1];
  if (from === undefined) return [];
  const to = lineEnd(index, line);
  const calls: SyntaxNode[] = [];
  index.tree.iterate({
    from,
    to,
    enter: (node: { name: string; from: number; node: SyntaxNode }) => {
      if (node.name === "CallExpression" && node.from >= from && node.from <= to)
        calls.push(node.node);
    },
  });
  return calls.sort((a, b) => a.from - b.from);
}

/**
 * The call expression a callsite points at: the one whose callee's last name is at the position,
 * else the first call on the line with one of `names` as its callee's last name. `null` when the
 * line has no such call (a callsite recorded by another version of the script).
 */
export function resolveCall(
  index: SourceIndex,
  callsite: Callsite,
  names: readonly string[] = [],
): CallSpan | null {
  const start = index.lineStarts[callsite.line - 1];
  if (start === undefined) return null;
  const pos = Math.min(start + Math.max(0, callsite.column - 1), lineEnd(index, callsite.line));
  for (let node: SyntaxNode | null = index.tree.resolveInner(pos, 1); node; node = node.parent) {
    if (node.name !== "CallExpression") continue;
    const name = calleeName(node.firstChild);
    if (name && name.from <= pos && pos <= name.to) {
      const span = callSpan(index, node);
      if (span && (names.length === 0 || names.includes(span.name))) return span;
    }
  }
  const calls = callsOnLine(index, callsite.line)
    .map((n) => callSpan(index, n))
    .filter((s): s is CallSpan => s !== null);
  return (
    calls.find((c) => names.includes(c.name)) ?? (names.length === 0 ? calls[0] : null) ?? null
  );
}

/** The names a callsite of an activity can point at. */
export function calleeNames(
  activity: { kind: "call"; action: string } | { kind: "timer" | "signal" | "log" },
): string[] {
  switch (activity.kind) {
    case "call":
      return [activity.action.slice(activity.action.lastIndexOf(".") + 1)];
    case "timer":
      return ["sleep", "setTimeout", "signal"];
    case "signal":
      return ["signal"];
    case "log":
      return ["log", "info", "warn", "error", "debug"];
  }
}

export interface Binding {
  name: string;
  /** Where the name is declared. */
  def: Span;
  /** The field of the value it is bound to: one level of destructuring. */
  path: string | number | null;
  /** Every place the name is read in its scope, the declaration first. */
  refs: Span[];
}

const SCOPES = new Set(["Block", "Script", "ArrowFunction", "FunctionDeclaration"]);

/** The scope a declaration is visible in: its block, or the function or script around it. */
function scopeOf(node: SyntaxNode): SyntaxNode | null {
  for (let n = node.parent; n; n = n.parent) if (SCOPES.has(n.name)) return n;
  return null;
}

/** The places a name is read between `from` and the end of its scope. */
function references(index: SourceIndex, name: string, def: SyntaxNode): Span[] {
  const scope = scopeOf(def);
  const refs: Span[] = [{ from: def.from, to: def.to }];
  if (!scope) return refs;
  index.tree.iterate({
    from: def.to,
    to: scope.to,
    enter: (ref: { name: string; from: number; to: number; node: SyntaxNode }) => {
      if (index.text.slice(ref.from, ref.to) !== name) return;
      // A name read, or a shorthand property (`{ issues }`), which reads it too.
      const shorthand =
        ref.name === "PropertyDefinition" &&
        ref.node.parent?.name === "Property" &&
        ref.node.parent.firstChild?.from === ref.node.parent.lastChild?.from;
      if (ref.name === "VariableName" || shorthand) refs.push({ from: ref.from, to: ref.to });
    },
  });
  return refs;
}

/** The names a pattern binds: a name, or one level of object or array destructuring. */
function patternBindings(index: SourceIndex, pattern: SyntaxNode): Binding[] {
  const text = (n: SyntaxNode) => index.text.slice(n.from, n.to);
  const bind = (def: SyntaxNode, path: string | number | null): Binding => ({
    name: text(def),
    def: { from: def.from, to: def.to },
    path,
    refs: references(index, text(def), def),
  });
  if (pattern.name === "VariableDefinition") return [bind(pattern, null)];
  const out: Binding[] = [];
  if (pattern.name === "ObjectPattern") {
    for (let p = pattern.firstChild; p; p = p.nextSibling) {
      if (p.name !== "PatternProperty") continue;
      const key = p.getChild("PropertyName");
      const def = p.getChild("VariableDefinition");
      if (def) out.push(bind(def, key ? text(key) : text(def)));
    }
  } else if (pattern.name === "ArrayPattern") {
    let i = 0;
    for (let p = pattern.firstChild; p; p = p.nextSibling) {
      if (p.name === ",") i += 1;
      if (p.name === "VariableDefinition") out.push(bind(p, i));
    }
  }
  return out;
}

const PATTERNS = new Set(["VariableDefinition", "ObjectPattern", "ArrayPattern"]);

/**
 * The names bound to a call's awaited result: `const x = await call()`, `const { a, b: c } =
 * await call()`, `const [a, b] = await call()`, also through parentheses and `as`.
 */
export function bindingsOf(index: SourceIndex, call: CallSpan): Binding[] {
  let node: SyntaxNode | null = index.tree.resolveInner(call.from, 1);
  while (
    node &&
    !(node.name === "CallExpression" && node.from === call.from && node.to === call.to)
  )
    node = node.parent;
  if (!node) return [];
  let awaited = false;
  let expr: SyntaxNode = node;
  for (let up = node.parent; up; up = up.parent) {
    if (up.name === "AwaitExpression") awaited = true;
    else if (up.name === "ParenthesizedExpression") {
      // (await call())
    } else if (up.name === "BinaryExpression" && up.getChild("as")) {
      // (await call()) as T
    } else break;
    expr = up;
  }
  const declaration = expr.parent;
  if (!awaited || declaration?.name !== "VariableDeclaration") return [];
  let pattern: SyntaxNode | null = expr.prevSibling;
  while (pattern && !PATTERNS.has(pattern.name)) {
    if (pattern.name !== "Equals" && pattern.name !== "TypeAnnotation") return [];
    pattern = pattern.prevSibling;
  }
  return pattern ? patternBindings(index, pattern) : [];
}

/** The function the run starts in: the default export, else a function named `main`. */
function mainFunction(index: SourceIndex): SyntaxNode | null {
  const top = index.tree.topNode;
  for (let n = top.firstChild; n; n = n.nextSibling) {
    if (n.name === "ExportDeclaration" && n.getChild("default")) {
      const fn =
        n.getChild("FunctionDeclaration") ??
        n.getChild("ArrowFunction") ??
        n.getChild("FunctionExpression");
      if (fn) return fn;
    }
  }
  for (let n = top.firstChild; n; n = n.nextSibling) {
    const fn = n.name === "ExportDeclaration" ? n.getChild("FunctionDeclaration") : n;
    if (fn?.name === "FunctionDeclaration") {
      const name = fn.getChild("VariableDefinition");
      if (name && index.text.slice(name.from, name.to) === "main") return fn;
    }
  }
  return null;
}

/** The line the run starts at: its main function's. */
export function startLine(index: SourceIndex): number | null {
  const fn = mainFunction(index);
  return fn ? lineAt(index, fn.from) : null;
}

/** The names bound to the run's input: the main function's first parameter. */
export function inputBindings(index: SourceIndex): Binding[] {
  const params = mainFunction(index)?.getChild("ParamList");
  let first = params?.firstChild ?? null;
  while (first && !PATTERNS.has(first.name)) first = first.nextSibling;
  return first ? patternBindings(index, first) : [];
}
