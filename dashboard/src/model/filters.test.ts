import { describe, expect, it } from "vitest";
import {
  actionsSearch,
  legacyActionsQuery,
  legacyNamespacesQuery,
  legacyWorkflowsQuery,
  namespacesSearch,
  toActionStatsQuery,
  workflowsSearch,
} from "./filters";

const NOW = Date.parse("2026-11-12T10:00:00Z");

describe("the Workflows' search", () => {
  const keep = (q: string, actions: string[]) => workflowsSearch(q).keeps(actions);

  it("asks for every workflow, and keeps those that called an action", () => {
    const all = workflowsSearch("");
    expect(all.query).toEqual({ limit: 50 });
    expect(all.namespace).toBeNull();
    expect(all.filtered).toBe(false);
    expect(all.fault).toBeNull();
    expect(keep("action:github.close_issue", ["github.close_issue", "slack.post_message"])).toBe(
      true,
    );
    expect(keep("action:github.close_issue", ["github.list_issues"])).toBe(false);
    expect(keep("action:github.*", ["github.list_issues"])).toBe(true);
    expect(keep("-action:github.*", ["github.list_issues"])).toBe(false);
    expect(keep("action:ask_user,crm.*", ["crm.provision"])).toBe(true);
    expect(workflowsSearch("action:github.*").filtered).toBe(true);
  });

  it("narrows to one namespace, which the header names and the counts take", () => {
    const search = workflowsSearch("namespace:alice action:crm.provision");
    expect(search.query).toEqual({ limit: 50, q: "namespace:alice" });
    expect(search.namespace).toBe("alice");
    expect(workflowsSearch('namespace:"user@example.com"').namespace).toBe("user@example.com");
    expect(workflowsSearch("namespace:alice,bob").fault?.message).toMatch(/one end user/);
    expect(workflowsSearch("namespace:ali*").fault?.message).toMatch(/one end user/);
    expect(workflowsSearch('namespace:"a b"').fault?.message).toMatch(/letters, digits/);
    expect(workflowsSearch("namespace:a namespace:b").fault).toMatchObject({ start: 12, end: 23 });
    expect(workflowsSearch("-namespace:alice").fault?.message).toMatch(/excluded/);
  });

  it("underlines what the list cannot read, in characters: labels among them", () => {
    expect(workflowsSearch("action:x status:failed").fault).toMatchObject({ start: 9, end: 22 });
    expect(workflowsSearch("namespace:alice @agent:triage").fault).toMatchObject({
      start: 16,
      end: 29,
      message: expect.stringMatching(/know no labels/),
    });
    expect(workflowsSearch("é action:x").fault).toMatchObject({ start: 0, end: 1 });
    expect(workflowsSearch("action:").fault?.message).toMatch(/needs an action/);
  });

  it("reads the filters of older links as a query, a tool as an action", () => {
    expect(legacyWorkflowsQuery(new URLSearchParams("label=agent:triage&tool=x"))).toBe(
      "@agent:triage action:x",
    );
    expect(legacyWorkflowsQuery(new URLSearchParams("q=action:x"))).toBeNull();
  });
});

describe("the Actions' search", () => {
  const keep = (q: string, actions: string[]) => actions.filter(actionsSearch(q).keeps);
  const ACTIONS = ["github.close_issue", "github.list_issues", "slack.post_message", "ask_user"];

  it("keeps the actions a query names, by name, group or text, or excludes them", () => {
    expect(keep("", ACTIONS)).toEqual(ACTIONS);
    expect(keep("action:github.*", ACTIONS)).toEqual(["github.close_issue", "github.list_issues"]);
    expect(keep("action:ask_user,slack.post_message", ACTIONS)).toEqual([
      "slack.post_message",
      "ask_user",
    ]);
    expect(keep("-action:github.*", ACTIONS)).toEqual(["slack.post_message", "ask_user"]);
    expect(keep("ISSUE", ACTIONS)).toEqual(["github.close_issue", "github.list_issues"]);
    expect(keep('"list_" -slack', ACTIONS)).toEqual(["github.list_issues"]);
  });

  it("asks the server for one namespace, and one group", () => {
    const search = actionsSearch("namespace:alice action:github.*");
    expect(search.namespace).toBe("alice");
    expect(search.group).toBe("github");
    expect(actionsSearch("action:github.close_issue").group).toBeNull();
    expect(actionsSearch("action:github.*,slack.*").group).toBeNull();
    expect(actionsSearch("action:github.* action:slack.*").group).toBeNull();
    expect(actionsSearch("-action:github.*").group).toBeNull();
    expect(actionsSearch("namespace:a,b").fault?.message).toMatch(/one end user/);
    expect(actionsSearch("-namespace:a").fault?.message).toMatch(/excluded/);
    expect(actionsSearch("@agent:triage").fault).toMatchObject({ start: 0, end: 13 });
    // Counters keep no action set: an older link says so.
    expect(legacyActionsQuery(new URLSearchParams("toolset=079b3789"))).toBe("action_set:079b3789");
    expect(actionsSearch("action_set:079b3789").fault?.message).toMatch(/not per action set/);
  });

  it("reads a window of the last 24 hours, of a namespace and a group", () => {
    expect(toActionStatsQuery("24h", { namespace: null, group: null }, NOW)).toEqual({
      since: "2026-11-11T10:00:00.000Z",
      until: "2026-11-12T10:00:00.000Z",
    });
    expect(toActionStatsQuery("1h", { namespace: "alice", group: "github" }, NOW)).toEqual({
      since: "2026-11-12T09:00:00.000Z",
      until: "2026-11-12T10:00:00.000Z",
      q: "namespace:alice",
      group: "github",
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
