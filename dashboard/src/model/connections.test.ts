import { describe, expect, it } from "vitest";
import type { Connection, ConsentRequest } from "../api/connections";
import {
  arrivedSince,
  firstMessage,
  hostOf,
  hostTools,
  initialChoices,
  labelsText,
  namespaceProblem,
  snapshotOf,
  toolsetTitle,
} from "./connections";

function connection(id: string, createdAt: string, lastUsedAt: string | null): Connection {
  return {
    id,
    person: { id: "user_01ADA", email: "ada@example.net", name: "Ada Lovelace" },
    client: {
      id: "https://claude.ai/oauth/mcp-oauth-client-metadata",
      name: "Claude",
      uri: "https://claude.ai",
      kind: "metadata_document",
    },
    organizations: [],
    createdAt,
    lastUsedAt,
  };
}

describe("hostTools", () => {
  it("keeps the tools of no server and no built-in", () => {
    expect(
      hostTools({
        id: "a".repeat(64),
        servers: [{ name: "crm", tools: 2, transport: "http" }],
        tools: [
          "ask_user",
          "crm.find",
          "crm.update",
          "http",
          "llm.summarize",
          "workflows.triage.start",
        ],
      }),
    ).toEqual(["ask_user", "llm.summarize"]);
  });

  it("names a toolset by its servers and host-tool groups", () => {
    expect(
      toolsetTitle({
        id: "b".repeat(64),
        servers: [{ name: "slack", tools: 1, transport: "http" }],
        tools: ["llm.summarize", "slack.post_message"],
      }),
    ).toBe("llm, slack");
  });
});

describe("firstMessage", () => {
  it("names the organization, the toolset and the namespace", () => {
    const text = firstMessage({ organization: "Acme", toolset: "3f9a", namespace: "demo" });
    expect(text.startsWith("With Rewind, org Acme, toolset 3f9a, namespace demo: ")).toBe(true);
  });

  it("leaves out a toolset not yet registered, and falls back to demo", () => {
    expect(firstMessage({ organization: "Acme", toolset: null, namespace: " " })).toMatch(
      /^With Rewind, org Acme, namespace demo: /,
    );
  });

  it("checks a namespace's name", () => {
    expect(namespaceProblem("user_7Kd02@acme:eu")).toBeNull();
    expect(namespaceProblem("bob smith")).not.toBeNull();
  });
});

describe("arrivedSince", () => {
  const before = [connection("cn_1", "2026-11-10T10:00:00Z", "2026-11-18T10:00:00Z")];

  it("sees nothing while nothing moved", () => {
    expect(arrivedSince(snapshotOf(before), before)).toBeNull();
  });

  it("sees a new connection", () => {
    const next = [...before, connection("cn_2", "2026-11-19T10:00:00Z", null)];
    expect(arrivedSince(snapshotOf(before), next)?.id).toBe("cn_2");
  });

  it("does not take a connection never used for one used since", () => {
    const unused = connection("cn_3", "2026-11-19T10:00:00Z", null);
    const { lastUsedAt: _, ...withoutLastUse } = unused;
    expect(arrivedSince(snapshotOf([unused]), [withoutLastUse])).toBeNull();
  });

  it("sees a connection used since", () => {
    const next = [connection("cn_1", "2026-11-10T10:00:00Z", "2026-11-19T10:00:05Z")];
    expect(arrivedSince(snapshotOf(before), next)?.id).toBe("cn_1");
  });
});

describe("initialChoices", () => {
  const request: ConsentRequest = {
    client: { id: "c", name: "Claude", uri: null, logoUri: null, kind: "metadata_document" },
    redirectHost: "claude.ai",
    loopback: false,
    allowedLabels: { via: "claude" },
    expiresAt: "2026-11-19T10:10:00Z",
    organizations: [
      { id: "o1", name: "Acme", role: "owner", accesses: ["read", "run"], current: true },
      { id: "o2", name: "Globex", role: "reader", accesses: ["read"], current: false },
      { id: "o3", name: "Initech", role: "worker", accesses: [], current: false },
    ],
  };

  it("ticks the current organization with the widest access it may give", () => {
    const choices = initialChoices(request);
    expect(choices.get("o1")).toEqual({ checked: true, access: "run" });
    expect(choices.get("o2")).toEqual({ checked: false, access: "read" });
    expect(choices.get("o3")?.checked).toBe(false);
  });

  it("never ticks a current organization the person may give nothing in", () => {
    const choices = initialChoices({
      ...request,
      organizations: [{ id: "o3", name: "Initech", role: "worker", accesses: [], current: true }],
    });
    expect(choices.get("o3")?.checked).toBe(false);
  });
});

describe("formats", () => {
  it("writes labels and hosts", () => {
    expect(labelsText({ via: "claude", env: "dev" })).toBe("env=dev, via=claude");
    expect(hostOf("https://claude.ai/api/mcp")).toBe("claude.ai");
    expect(hostOf(null)).toBeNull();
  });
});
