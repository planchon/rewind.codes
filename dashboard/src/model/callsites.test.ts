import { describe, expect, it } from "vitest";
import { CODES } from "../mocks/fixtures";
import {
  bindingsOf,
  indexSource,
  inputBindings,
  lineAt,
  resolveCall,
  startLine,
} from "./callsites";

const index = indexSource(CODES.TRIAGE_WORKFLOW.source);
const text = (s: { from: number; to: number }) => index.text.slice(s.from, s.to);

describe("callsites", () => {
  it("lead from the callee's last name to its call expression", () => {
    // The callsites the runtime recorded for the corpus run.
    const list = resolveCall(index, { line: 5, column: 31 }, ["list_issues"]);
    expect(list && text(list)).toBe('github.list_issues({ repo: input.repo, state: "open" })');
    expect(list?.name).toBe("list_issues");
    const post = resolveCall(index, { line: 8, column: 15 });
    expect(post && text(post)).toMatch(/^slack\.post_message\(\{ channel: "#triage"/);
    const signal = resolveCall(index, { line: 10, column: 26 }, ["signal"]);
    expect(signal && text(signal)).toBe(
      'signal<{ close: boolean }>("approval", { timeout: "3d" })',
    );
    // Inside Promise.all and an arrow function: the inner call, not Promise.all.
    const close = resolveCall(index, { line: 13, column: 45 }, ["close_issue"]);
    expect(close && text(close)).toBe("github.close_issue({ repo: input.repo, number: i.number })");
    const sleep = resolveCall(index, { line: 15, column: 9 }, ["sleep"]);
    expect(sleep && text(sleep)).toBe('sleep("10d")');
  });

  it("fall back to the call of that name on the line when the column misses it", () => {
    const list = resolveCall(index, { line: 5, column: 3 }, ["list_issues"]);
    expect(list && text(list)).toMatch(/^github\.list_issues/);
    expect(resolveCall(index, { line: 5, column: 3 }, ["close_issue"])).toBeNull();
    expect(resolveCall(index, { line: 99, column: 1 }, ["x"])).toBeNull();
  });

  it("find the names bound to an awaited call, and where they are read", () => {
    const list = resolveCall(index, { line: 5, column: 31 }, ["list_issues"]);
    if (!list) throw new Error("no call");
    const [issues] = bindingsOf(index, list);
    expect(issues?.name).toBe("issues");
    expect(issues?.path).toBeNull();
    expect(issues?.refs.map((r) => lineAt(index, r.from))).toEqual([5, 6]);

    const signal = resolveCall(index, { line: 10, column: 26 }, ["signal"]);
    if (!signal) throw new Error("no call");
    const [decision] = bindingsOf(index, signal);
    expect(decision?.refs.map((r) => lineAt(index, r.from))).toEqual([10, 11]);

    // Not bound: awaited for its effect only.
    const post = resolveCall(index, { line: 8, column: 15 });
    if (!post) throw new Error("no call");
    expect(bindingsOf(index, post)).toEqual([]);
  });

  it("read one level of destructuring, parentheses and as", () => {
    const source = `export default async function main({ repo, limit }: Input) {
  const { items, next: cursor } = await api.page({ repo });
  const [first, , third] = await api.list();
  const total = (await api.count()) as number;
  const pending = api.later();
  return { items, cursor, first, third, total, pending, repo };
}
`;
    const ix = indexSource(source);
    const bound = (line: number, name: string) => {
      const call = resolveCall(ix, { line, column: 1 }, [name]);
      if (!call) throw new Error(`no call ${name}`);
      return bindingsOf(ix, call).map((b) => [b.name, b.path, b.refs.length]);
    };
    expect(bound(2, "page")).toEqual([
      ["items", "items", 2],
      ["cursor", "next", 2],
    ]);
    expect(bound(3, "list")).toEqual([
      ["first", 0, 2],
      ["third", 2, 2],
    ]);
    expect(bound(4, "count")).toEqual([["total", null, 2]]);
    // A promise is not its result.
    expect(bound(5, "later")).toEqual([]);
    expect(inputBindings(ix).map((b) => [b.name, b.path])).toEqual([
      ["repo", "repo"],
      ["limit", "limit"],
    ]);
  });

  it("bind the run's input to the main function's parameter", () => {
    expect(startLine(index)).toBe(4);
    const [input] = inputBindings(index);
    expect(input?.name).toBe("input");
    expect(input?.refs.map((r) => lineAt(index, r.from))).toEqual([4, 5, 8, 13, 16]);
  });
});
