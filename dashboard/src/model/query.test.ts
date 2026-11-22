import { describe, expect, it } from "vitest";
import {
  askedTerms,
  countTerms,
  facetState,
  facetValues,
  legacyQuery,
  quoteValue,
  splitTerms,
  suggest,
  TEXT_VALUES,
  toggleFacet,
  typedField,
  utf16Span,
  withFieldTerm,
  withQuery,
  withTerm,
} from "./query";
import { ascending, readSort, withSort } from "./sorts";
import type { Facets } from "./types";

const FACETS: Facets = {
  statuses: [
    { value: "completed", runs: 60 },
    { value: "failed", runs: 7 },
  ],
  labels: [
    {
      key: "agent",
      runs: 80,
      values: [
        { value: "support", runs: 70 },
        { value: "triage", runs: 6 },
      ],
    },
    { key: "env", runs: 80, values: [{ value: "prod", runs: 75 }] },
  ],
  tools: [
    { value: "weather.forecast", runs: 64 },
    { value: "github.close_issue", runs: 4 },
  ],
  errors: [{ value: "ToolError", runs: 6 }],
  scripts: [],
  waiting: [{ value: "signal", runs: 1 }],
  signals: [{ value: "approval", runs: 1 }],
};

describe("the query", () => {
  it("splits into terms, a quoted text kept whole, with their positions", () => {
    const q = '@agent:triage -status:completed "rate limit" tool:github.*';
    expect(
      splitTerms(q).map((t) => [t.kind, t.negated, t.field, t.operand, q.slice(t.start, t.end)]),
    ).toEqual([
      ["label", false, "agent", "triage", "@agent:triage"],
      ["field", true, "status", "completed", "-status:completed"],
      ["text", false, null, '"rate limit"', '"rate limit"'],
      ["field", false, "tool", "github.*", "tool:github.*"],
    ]);
  });

  it("quotes a value only when it must", () => {
    expect(quoteValue("github.close_issue")).toBe("github.close_issue");
    expect(quoteValue("data platform")).toBe('"data platform"');
    expect(quoteValue('a"b')).toBe('"a\\"b"');
    expect(quoteValue("a*")).toBe('"a*"');
  });

  it("adds, excludes then removes a facet's value", () => {
    let q = "@env:prod";
    q = toggleFacet(q, "status", "failed");
    expect(q).toBe("@env:prod status:failed");
    expect(facetState(q, "status", "failed")).toBe("include");
    q = toggleFacet(q, "status", "failed");
    expect(q).toBe("@env:prod -status:failed");
    expect(facetState(q, "status", "failed")).toBe("exclude");
    q = toggleFacet(q, "status", "failed");
    expect(q).toBe("@env:prod");
    expect(facetState(q, "status", "failed")).toBeNull();
  });

  it("toggles one value of a list, and a label's value", () => {
    const q = "status:failed,aborted";
    expect(facetState(q, "status", "aborted")).toBe("include");
    expect(toggleFacet(q, "status", "aborted")).toBe("status:failed -status:aborted");
    expect(toggleFacet("", "@team", "data platform")).toBe('@team:"data platform"');
    expect(facetValues("-@env:test @env:prod", "@env")).toEqual([
      { value: "test", state: "exclude" },
      { value: "prod", state: "include" },
    ]);
  });

  it("adds a term once", () => {
    expect(withTerm("status:failed", "status:failed")).toBe("status:failed");
    expect(withTerm("  ", "waiting:*")).toBe("waiting:*");
  });

  it("turns the server's span in characters into one in UTF-16 units", () => {
    const q = "@who:😀 stauts:failed";
    expect(utf16Span(q, 7, 20)).toEqual([8, 21]);
    expect(q.slice(8, 21)).toBe("stauts:failed");
  });
});

describe("suggestions", () => {
  it("offer the fields and the label keys while a name is typed", () => {
    const s = suggest("sta", 3, FACETS);
    expect(s?.from).toBe(0);
    expect(s?.items.map((i) => i.insert)).toEqual(["status:", "started:", "started_by:"]);
    const labels = suggest("@a", 2, FACETS);
    expect(labels?.items.map((i) => i.insert)).toEqual(["@agent:"]);
    expect(suggest("", 0, FACETS)?.items.length).toBeGreaterThan(5);
  });

  it("offer the values that exist, with their counts", () => {
    const status = suggest("status:f", 8, FACETS);
    expect(status?.items.map((i) => [i.insert, i.count])).toEqual([["status:failed", 7]]);
    const agent = suggest("@agent:", 7, FACETS);
    expect(agent?.items.map((i) => i.insert)).toEqual(["@agent:support", "@agent:triage"]);
    const tool = suggest("x tool:gi", 9, FACETS);
    expect(tool?.from).toBe(2);
    expect(tool?.items.map((i) => i.insert)).toEqual(["tool:github.close_issue", "tool:github.*"]);
    // After a comma, the next value; and after a minus, the term without it.
    const list = suggest("-waiting:signal,", 16, FACETS);
    expect(list?.from).toBe(1);
    expect(list?.items.map((i) => i.insert)).toContain("waiting:signal,timer");
  });

  it("offer nothing inside quoted text", () => {
    expect(suggest('"rate', 5, FACETS)).toBeNull();
  });

  it("offer the namespaces of the facets, then those looked up", () => {
    const facets = {
      ...FACETS,
      namespaces: [
        { value: "bob", runs: 26 },
        { value: "alice", runs: 17 },
      ],
    };
    const values = { namespace: [{ value: "bob" }, { value: "ben", detail: "3 runs this month" }] };
    const s = suggest("namespace:", 10, facets, 12, values);
    expect(s?.items.map((i) => [i.insert, i.count ?? i.detail])).toEqual([
      ["namespace:bob", 26],
      ["namespace:alice", 17],
      ["namespace:ben", "3 runs this month"],
    ]);
    expect(suggest("namespace:b", 11, facets, 12, values)?.items.map((i) => i.insert)).toEqual([
      "namespace:bob",
      "namespace:ben",
    ]);
  });

  it("follow the language of the view, and offer values for free text", () => {
    const language = { fields: [{ name: "tool", doc: "" }], labels: false };
    expect(suggest("t", 1, FACETS, 12, undefined, language)?.items.map((i) => i.insert)).toEqual([
      "tool:",
    ]);
    expect(suggest("@a", 2, FACETS, 12, undefined, language)).toBeNull();
    expect(suggest("status:f", 8, FACETS, 12, undefined, language)).toBeNull();
    const names = { [TEXT_VALUES]: [{ value: "user_7Kd02" }, { value: "dana" }] };
    expect(suggest("us", 2, undefined, 12, names, language)?.items.map((i) => i.insert)).toEqual([
      "user_7Kd02",
    ]);
    expect(typedField("x us", 4)).toEqual({ field: TEXT_VALUES, value: "us" });
    expect(typedField("namespace:b", 11)).toEqual({ field: "namespace", value: "b" });
  });
});

describe("older links", () => {
  it("become a query", () => {
    const params = new URLSearchParams(
      "status=failed&label=agent:triage&label=team:data platform&tool=github.close_issue&error=timeout&quarantined=no&range=7d",
    );
    expect(legacyQuery(params)).toBe(
      '@agent:triage @team:"data platform" status:failed tool:github.close_issue error:timeout quarantined:no started:>now-7d'.replace(
        /^(.*?) (status:failed) (.*)$/,
        "$2 $1 $3",
      ),
    );
    expect(
      legacyQuery(new URLSearchParams("q=@env:prod&parent=x&after=2026-11-01T00:00:00Z")),
    ).toBe("@env:prod parent:x started:>=2026-11-01T00:00:00Z");
    expect(legacyQuery(new URLSearchParams("q=@env:prod"))).toBeNull();
    expect(withQuery(new URLSearchParams("status=failed&step=3"), "status:failed").toString()).toBe(
      "step=3&q=status%3Afailed",
    );
  });
});

describe("a filter that takes one value at a time", () => {
  it("replaces the terms that ask for its field, and leaves the excluded ones", () => {
    expect(withFieldTerm("status:failed duration:>1m", "duration", "duration:>1h")).toBe(
      "status:failed duration:>1h",
    );
    expect(withFieldTerm("duration:>1m -duration:>1h", "duration", null)).toBe("-duration:>1h");
    expect(withFieldTerm("", "calls", "calls:0")).toBe("calls:0");
    expect(askedTerms("-errors:0 errors:>0", "errors").map((t) => t.raw)).toEqual(["errors:>0"]);
  });

  it("counts the terms of a column's fields, every label for @", () => {
    const q = '@agent:triage -status:completed status:failed "rate limit" quarantined:yes';
    expect(countTerms(q, ["status", "quarantined"])).toBe(3);
    expect(countTerms(q, ["@"])).toBe(1);
    expect(countTerms(q, ["duration"])).toBe(0);
  });
});

describe("the orders of the runs", () => {
  it("reads the URL's order, the newest first by default, and leaves the default out", () => {
    expect(readSort(new URLSearchParams("sort=duration"))).toBe("duration");
    expect(readSort(new URLSearchParams("sort=fastest"))).toBe("started");
    expect(withSort(new URLSearchParams("q=a&sort=calls"), "started").toString()).toBe("q=a");
    expect(withSort(new URLSearchParams("q=a"), "oldest").toString()).toBe("q=a&sort=oldest");
    expect(ascending("oldest")).toBe(true);
    expect(ascending("duration")).toBe(false);
  });
});
