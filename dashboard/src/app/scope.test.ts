import { describe, expect, it } from "vitest";
import { isNamespaceName, scopedPath, splitScope } from "../state/scope";
import { switchedPath, unscopedPath, withoutNamespaceTerms } from "./scope";

describe("the scope's paths", () => {
  it("puts a view in a namespace's scope, and takes it out", () => {
    expect(scopedPath("/observability/failures", "alice")).toBe("/ns/alice/observability/failures");
    expect(scopedPath("/", "alice")).toBe("/ns/alice");
    expect(scopedPath("/overview", null)).toBe("/overview");
    expect(scopedPath("/overview", "a@b.c")).toBe("/ns/a%40b.c/overview");
    expect(splitScope("/ns/a%40b.c/overview")).toEqual({ namespace: "a@b.c", path: "/overview" });
    expect(splitScope("/ns/alice")).toEqual({ namespace: "alice", path: "/" });
    expect(splitScope("/observability")).toEqual({ namespace: null, path: "/observability" });
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
        "/observability/failures",
        "?q=namespace%3Abob+status%3Afailed&window=7d&sort=duration&other=1",
        "alice",
      ),
    ).toBe("/ns/alice/observability/failures?q=status%3Afailed&window=7d&sort=duration");
    expect(switchedPath("/ns/alice/overview", "?window=1h", null)).toBe("/overview?window=1h");
    expect(switchedPath("/ns/alice/tools", "", "bob")).toBe("/ns/bob/tools");
    // Tools and Scripts have no window: they keep their query only.
    expect(switchedPath("/tools", "?q=tool%3Agithub.*&window=7d", "bob")).toBe(
      "/ns/bob/tools?q=tool%3Agithub.*",
    );
  });

  it("opens the list of an object's kind, and exchanges Namespaces and Usage", () => {
    expect(switchedPath("/runs/alice/triage/06h0x1tsa0qx", "", "bob")).toBe(
      "/ns/bob/observability",
    );
    expect(switchedPath("/scripts/ab12", "?q=x", "bob")).toBe("/ns/bob/scripts");
    expect(switchedPath("/testing/triage/tests/ab", "", null)).toBe("/testing");
    expect(switchedPath("/namespaces", "?order=runs", "bob")).toBe("/ns/bob/usage");
    expect(switchedPath("/ns/bob/usage", "?month=2026-10", null)).toBe("/namespaces");
    expect(switchedPath("/ns/bob/secrets", "", "alice")).toBe("/ns/alice/secrets");
  });

  it("leads the sidebar's namespace back to the same view over every namespace", () => {
    expect(unscopedPath("/ns/bob/observability/waiting", "?q=%40env%3Aprod")).toBe(
      "/observability/waiting?q=%40env%3Aprod",
    );
    expect(unscopedPath("/runs/bob/triage/06h0x1tsa0qx", "")).toBe("/observability");
  });

  it("drops a query's namespace terms, whatever their form", () => {
    expect(withoutNamespaceTerms("namespace:bob status:failed -namespace:alice @env:prod")).toBe(
      "status:failed @env:prod",
    );
    expect(withoutNamespaceTerms("namespace:bob")).toBe("");
  });
});
