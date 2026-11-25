import { describe, expect, it } from "vitest";
import { isNamespaceName, scopedPath, splitScope } from "../state/scope";
import { switchedPath, unscopedPath, withoutNamespaceTerms } from "./scope";

describe("the scope's paths", () => {
  it("puts a view in a namespace's scope, and takes it out", () => {
    expect(scopedPath("/runs/failures", "alice")).toBe("/ns/alice/runs/failures");
    expect(scopedPath("/", "alice")).toBe("/ns/alice");
    expect(scopedPath("/overview", null)).toBe("/overview");
    expect(scopedPath("/overview", "a@b.c")).toBe("/ns/a%40b.c/overview");
    expect(splitScope("/ns/a%40b.c/overview")).toEqual({ namespace: "a@b.c", path: "/overview" });
    expect(splitScope("/ns/alice")).toEqual({ namespace: "alice", path: "/" });
    expect(splitScope("/runs")).toEqual({ namespace: null, path: "/runs" });
  });

  it("takes a namespace's name as the API does", () => {
    expect(isNamespaceName("user_7Kd02")).toBe(true);
    expect(isNamespaceName("a:b@c.d-e")).toBe(true);
    expect(isNamespaceName("")).toBe(false);
    expect(isNamespaceName("a b")).toBe(false);
    expect(isNamespaceName("rewind:system")).toBe(false);
    expect(isNamespaceName("x".repeat(129))).toBe(false);
  });
});

describe("where the picker leads", () => {
  it("keeps the view, its query, window and order, without the namespaces the query named", () => {
    expect(
      switchedPath(
        "/runs/failures",
        "?q=namespace%3Abob+status%3Afailed&window=7d&sort=duration&other=1",
        "alice",
      ),
    ).toBe("/ns/alice/runs/failures?q=status%3Afailed&window=7d&sort=duration");
    expect(switchedPath("/ns/alice/overview", "?window=1h", null)).toBe("/overview?window=1h");
    expect(switchedPath("/ns/alice/actions", "", "bob")).toBe("/ns/bob/actions");
    // Actions and Workflows have no window: they keep their query only.
    expect(switchedPath("/actions", "?q=action%3Agithub.*&window=7d", "bob")).toBe(
      "/ns/bob/actions?q=action%3Agithub.*",
    );
    expect(switchedPath("/workflows", "?q=action%3Agithub.*&window=7d", "bob")).toBe(
      "/ns/bob/workflows?q=action%3Agithub.*",
    );
  });

  it("opens the list of an object's kind, and exchanges Namespaces and Usage", () => {
    expect(switchedPath("/runs/alice/triage/06h0x1tsa0qx", "", "bob")).toBe("/ns/bob/runs");
    expect(switchedPath("/runs/0193c1a2-7d4e-7b11-9c40-5a1f3e2d8b60", "?q=x", null)).toBe("/runs");
    expect(switchedPath("/workflows/wf_ab12", "?q=x", "bob")).toBe("/ns/bob/workflows");
    expect(switchedPath("/workflows/wf_ab12/tests/ab", "", null)).toBe("/workflows");
    // The paths of earlier versions lead where they redirect.
    expect(switchedPath("/observability/active", "?q=x", "bob")).toBe("/ns/bob/runs/running?q=x");
    expect(switchedPath("/scripts/ab12", "?q=x", "bob")).toBe("/ns/bob/workflows");
    expect(switchedPath("/tools", "?q=x", null)).toBe("/actions?q=x");
    expect(switchedPath("/namespaces", "?order=runs", "bob")).toBe("/ns/bob/usage");
    expect(switchedPath("/ns/bob/usage", "?month=2026-10", null)).toBe("/namespaces");
    expect(switchedPath("/ns/bob/secrets", "", "alice")).toBe("/ns/alice/secrets");
  });

  it("leads the sidebar's namespace back to the same view over every namespace", () => {
    expect(unscopedPath("/ns/bob/runs/waiting", "?q=%40env%3Aprod")).toBe(
      "/runs/waiting?q=%40env%3Aprod",
    );
    expect(unscopedPath("/runs/bob/triage/06h0x1tsa0qx", "")).toBe("/runs");
  });

  it("drops a query's namespace terms, whatever their form", () => {
    expect(withoutNamespaceTerms("namespace:bob status:failed -namespace:alice @env:prod")).toBe(
      "status:failed @env:prod",
    );
    expect(withoutNamespaceTerms("namespace:bob")).toBe("");
  });
});
