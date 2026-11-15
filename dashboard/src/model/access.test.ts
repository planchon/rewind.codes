import { describe, expect, it } from "vitest";
import type { Identity, Role } from "../api/types";
import {
  canGive,
  displayName,
  effectivePermissions,
  formatLabels,
  groupPermissions,
  initials,
  missingPermissions,
  parseLabels,
  permissionsOf,
  roleNameProblem,
  splitPrincipal,
} from "./access";

const reader: Role = {
  name: "reader",
  builtIn: true,
  description: "",
  permissions: ["runs:read", "toolsets:read", "subscriptions:read", "members:read", "roles:read"],
};

describe("access", () => {
  it("drops the administration permissions of a grant limited to labels", () => {
    const identity: Identity = {
      operator: false,
      permissions: ["runs:read", "members:invite", "api_keys:read", "roles:write"],
      grant: { role: "custom", allowedLabels: { agent: "triage" } },
    };
    expect([...effectivePermissions(identity)]).toEqual(["runs:read"]);
    expect([...effectivePermissions({ ...identity, grant: { role: "custom" } })]).toEqual(
      identity.permissions,
    );
  });

  it("gives a role only to whoever holds all of its permissions", () => {
    const held = new Set(reader.permissions);
    expect(canGive(reader, held)).toBe(true);
    expect(canGive({ ...reader, permissions: ["runs:read", "runs:start"] }, held)).toBe(false);
    expect(missingPermissions(["runs:start", "runs:read"], held)).toEqual(["runs:start"]);
    expect(permissionsOf("operator", [])?.length).toBe(20);
    expect(permissionsOf("gone", [reader])).toBeUndefined();
  });

  it("checks a role's name as the server does", () => {
    expect(roleNameProblem("on-call_2")).toBeNull();
    expect(roleNameProblem("")).toMatch(/name/);
    expect(roleNameProblem("On-call")).toMatch(/lowercase/);
    expect(roleNameProblem("x".repeat(65))).toMatch(/64/);
    expect(roleNameProblem("owner")).toMatch(/built-in/);
    expect(roleNameProblem("operator")).toMatch(/built-in/);
  });

  it("reads and writes allowed labels as key:value pairs", () => {
    expect(parseLabels("agent:triage, conversation:c:42")).toEqual({
      labels: { agent: "triage", conversation: "c:42" },
    });
    expect(parseLabels("  ")).toEqual({ labels: {} });
    expect(parseLabels("agent")).toEqual({ error: "agent is not a label: write key:value." });
    expect(parseLabels("agent:")).toHaveProperty("error");
    expect(parseLabels("a:1 a:2")).toHaveProperty("error");
    expect(formatLabels({ z: "1", agent: "triage" })).toBe("agent:triage z:1");
  });

  it("groups the catalog by what it acts on", () => {
    const groups = groupPermissions([
      { permission: "runs:read" as const },
      { permission: "runs:start" as const },
      { permission: "api_keys:read" as const },
    ]);
    expect(groups.map((g) => [g.resource, g.items.length])).toEqual([
      ["runs", 2],
      ["api_keys", 1],
    ]);
  });

  it("names people and principals", () => {
    const ada = { id: "user_1", email: "ada@example.net", firstName: "Ada", lastName: "Lovelace" };
    expect(displayName(ada)).toBe("Ada Lovelace");
    expect(initials(ada)).toBe("AL");
    expect(displayName({ id: "user_2", email: "x@example.net" })).toBe("x@example.net");
    expect(initials({ id: "user_2", email: "x@example.net" })).toBe("X");
    expect(splitPrincipal("key:abc")).toEqual({ kind: "key", id: "abc" });
  });
});
