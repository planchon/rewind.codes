import { describe, expect, it } from "vitest";
import {
  legacyNamespacesQuery,
  legacyScriptsQuery,
  legacyToolsQuery,
  namespacesSearch,
  scriptsSearch,
  toolsSearch,
  toToolStatsQuery,
} from "./filters";

const NOW = Date.parse("2026-11-12T10:00:00Z");

describe("the Scripts' search", () => {
  it("sends its labels and one tool", () => {
    expect(scriptsSearch("@agent:triage @env:prod tool:github.close_issue")).toEqual({
      query: { limit: 50, label: ["agent:triage", "env:prod"], tool: "github.close_issue" },
      namespace: null,
      fault: null,
    });
    expect(scriptsSearch("")).toEqual({ query: { limit: 50 }, namespace: null, fault: null });
  });

  it("narrows to one namespace, which the header names", () => {
    expect(scriptsSearch("namespace:alice @agent:triage")).toEqual({
      query: { limit: 50, label: ["agent:triage"] },
      namespace: "alice",
      fault: null,
    });
    expect(scriptsSearch('namespace:"user@example.com"').namespace).toBe("user@example.com");
    expect(scriptsSearch("namespace:alice,bob").fault?.message).toMatch(/one end user/);
    expect(scriptsSearch("namespace:ali*").fault?.message).toMatch(/one end user/);
    expect(scriptsSearch('namespace:"a b"').fault?.message).toMatch(/letters, digits/);
    expect(scriptsSearch("namespace:a namespace:b").fault).toMatchObject({ start: 12, end: 23 });
    expect(scriptsSearch("-namespace:alice").fault?.message).toMatch(/excluded/);
  });

  it("underlines what the list cannot read, in characters", () => {
    expect(scriptsSearch("@agent:triage status:failed").fault).toMatchObject({
      start: 14,
      end: 27,
    });
    expect(scriptsSearch("é -@agent:triage").fault).toMatchObject({ start: 0, end: 1 });
    expect(scriptsSearch("-@agent:triage").fault?.message).toMatch(/excluded/);
    expect(scriptsSearch("tool:github.*").fault?.message).toMatch(/one tool/);
    expect(scriptsSearch("tool:a tool:b").fault).toMatchObject({ start: 7, end: 13 });
  });

  it("reads the filters of older links as a query", () => {
    expect(legacyScriptsQuery(new URLSearchParams("label=agent:triage&tool=x"))).toBe(
      "@agent:triage tool:x",
    );
    expect(legacyScriptsQuery(new URLSearchParams("q=tool:x"))).toBeNull();
  });
});

describe("the Tools' search", () => {
  const keep = (q: string, tools: string[]) => tools.filter(toolsSearch(q).keeps);
  const TOOLS = ["github.close_issue", "github.list_issues", "slack.post_message", "ask_user"];

  it("keeps the tools a query names, by name, pattern or text, or excludes them", () => {
    expect(keep("", TOOLS)).toEqual(TOOLS);
    expect(keep("tool:github.*", TOOLS)).toEqual(["github.close_issue", "github.list_issues"]);
    expect(keep("tool:ask_user,slack.post_message", TOOLS)).toEqual([
      "slack.post_message",
      "ask_user",
    ]);
    expect(keep("-tool:github.*", TOOLS)).toEqual(["slack.post_message", "ask_user"]);
    expect(keep("ISSUE", TOOLS)).toEqual(["github.close_issue", "github.list_issues"]);
    expect(keep('"list_" -slack', TOOLS)).toEqual(["github.list_issues"]);
  });

  it("asks the server for one namespace, and the start of the tools' names", () => {
    const search = toolsSearch("namespace:alice tool:github.*");
    expect(search.namespace).toBe("alice");
    expect(search.prefix).toBe("github.");
    expect(toolsSearch("tool:github.close_issue").prefix).toBeNull();
    expect(toolsSearch("tool:github.*,slack.*").prefix).toBeNull();
    expect(toolsSearch("tool:github.* tool:slack.*").prefix).toBeNull();
    expect(toolsSearch("-tool:github.*").prefix).toBeNull();
    expect(toolsSearch("namespace:a,b").fault?.message).toMatch(/one end user/);
    expect(toolsSearch("-namespace:a").fault?.message).toMatch(/excluded/);
    expect(toolsSearch("@agent:triage").fault).toMatchObject({ start: 0, end: 13 });
    // Counters keep no toolset: an older link says so.
    expect(legacyToolsQuery(new URLSearchParams("toolset=079b3789"))).toBe("toolset:079b3789");
    expect(toolsSearch("toolset:079b3789").fault?.message).toMatch(/not per toolset/);
  });

  it("reads a window of the last 24 hours, of a namespace and a prefix", () => {
    expect(toToolStatsQuery("24h", { namespace: null, prefix: null }, NOW)).toEqual({
      since: "2026-11-11T10:00:00.000Z",
      until: "2026-11-12T10:00:00.000Z",
    });
    expect(toToolStatsQuery("1h", { namespace: "alice", prefix: "github." }, NOW)).toEqual({
      since: "2026-11-12T09:00:00.000Z",
      until: "2026-11-12T10:00:00.000Z",
      q: "namespace:alice",
      prefix: "github.",
    });
  });
});

describe("the Namespaces' search", () => {
  it("reads the start of a name", () => {
    expect(namespacesSearch("user_7")).toEqual({ prefix: "user_7", fault: null });
    expect(namespacesSearch('"bob"')).toEqual({ prefix: "bob", fault: null });
    expect(namespacesSearch("")).toEqual({ prefix: null, fault: null });
    expect(namespacesSearch("bob alice").fault).toMatchObject({ start: 4, end: 9 });
    expect(namespacesSearch("status:failed").fault?.message).toMatch(/start of their name/);
    expect(legacyNamespacesQuery(new URLSearchParams("prefix=user_"))).toBe("user_");
  });
});
