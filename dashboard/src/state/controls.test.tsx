// The views read again from the change stream (archi/05, "Live"): a script, a workflow, a secret,
// a namespace or a connection that changes reads again the views that show it, and nothing else;
// a reopened stream reads once what is on screen.
import { screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { renderApp, signedIn } from "../test/app";
import { changeStream } from "../test/streams";

/** Lets the reads a change asked for leave, and those it did not ask for show they did not. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 300));

describe("a control-plane change", () => {
  it("reads again the secrets of its scope, and nothing else", async () => {
    const { requested } = renderApp("/secrets", {
      world: signedIn("ada@example.net", "org_01ACME"),
    });
    const changes = changeStream();
    await waitFor(() => expect(screen.getByText("org/slack")).toBeInTheDocument());
    await waitFor(() => expect(changes.live).toBe(true));
    await settle();
    const secrets = () => requested("/v1/secrets").length;
    const before = secrets();

    changes.control({ kind: "secret", name: "slack" });
    await waitFor(() => expect(secrets()).toBe(before + 1));

    // Another namespace's secret, a workflow, a namespace: the organization's list stays.
    changes.control({ kind: "secret", name: "github", namespace: "alice" });
    changes.control({ kind: "workflow", name: "triage", version: "ab" });
    changes.control({ kind: "namespace", name: "bob" });
    await settle();
    expect(secrets()).toBe(before + 1);
  });

  it("reads again the namespace it names, or the defaults, and not another", async () => {
    const { requested } = renderApp("/ns/bob/usage");
    const changes = changeStream();
    await screen.findByRole("img", { name: /^Cumulative CPU time of bob/ });
    await waitFor(() => expect(changes.live).toBe(true));
    await settle();
    const page = () => requested("/v1/namespaces/bob").length;
    const before = page();

    changes.control({ kind: "namespace", name: "alice" });
    changes.control({ kind: "script", digest: "ab" });
    await settle();
    expect(page()).toBe(before);

    changes.control({ kind: "namespace", name: "bob" });
    await waitFor(() => expect(page()).toBe(before + 1));
    changes.control({ kind: "namespace" });
    await waitFor(() => expect(page()).toBe(before + 2));
  });

  it("reads again the workflows when one has a new version", async () => {
    const { requested } = renderApp("/testing");
    const changes = changeStream();
    await screen.findByRole("table");
    await waitFor(() => expect(changes.live).toBe(true));
    await settle();
    const workflows = () => requested("/v1/workflows").length;
    const before = workflows();

    changes.control({ kind: "secret", name: "slack" });
    await settle();
    expect(workflows()).toBe(before);
    changes.control({ kind: "workflow", name: "triage", version: "ab" });
    await waitFor(() => expect(workflows()).toBe(before + 1));
  });

  it("reads again the scripts when the runs ran a new one", async () => {
    const { requested } = renderApp("/scripts");
    const changes = changeStream();
    await screen.findByRole("table");
    await waitFor(() => expect(changes.live).toBe(true));
    await settle();
    const scripts = () => requested("/v1/scripts").length;
    const before = scripts();

    changes.control({ kind: "connection", id: "conn_1" });
    await settle();
    expect(scripts()).toBe(before);
    changes.control({ kind: "script", digest: "ab" });
    await waitFor(() => expect(scripts()).toBe(before + 1));
  });

  it("reads again the connections of Connect Claude when one changes", async () => {
    const { requested } = renderApp("/connect", { world: signedIn("grace@example.net") });
    const changes = changeStream();
    await screen.findByRole("heading", { name: "Connect Claude" });
    await waitFor(() => expect(changes.live).toBe(true));
    await waitFor(() => expect(requested("/v1/connections").length).toBeGreaterThan(0));
    await settle();
    const connections = () => requested("/v1/connections").length;
    const before = connections();

    changes.control({ kind: "namespace", name: "bob" });
    await settle();
    expect(connections()).toBe(before);
    changes.control({ kind: "connection", id: "conn_1" });
    await waitFor(() => expect(connections()).toBe(before + 1));
  });
});

describe("a reopened change stream", () => {
  it("reads once what is on screen, and the views keep what they show meanwhile", async () => {
    const { requested } = renderApp("/observability");
    const changes = changeStream();
    const table = await screen.findByRole("table");
    await waitFor(() => expect(changes.live).toBe(true));
    await settle();
    const runs = () => requested("/v1/runs").length;
    const stats = () => requested("/v1/runs/stats").length;
    const [runsBefore, statsBefore] = [runs(), stats()];

    // Down: nothing is read, the table stays.
    changes.drop();
    await waitFor(() => expect(changes.live).toBe(false));
    expect(screen.getByRole("table")).toBe(table);
    expect([runs(), stats()]).toEqual([runsBefore, statsBefore]);

    // Back after its backoff (1 s, spread): what is on screen is read once.
    await waitFor(() => expect(changes.opened).toBe(2), { timeout: 4_000 });
    await waitFor(() => expect(runs()).toBe(runsBefore + 1));
    await waitFor(() => expect(stats()).toBeGreaterThan(statsBefore));
    await settle();
    const [runsAfter, statsAfter] = [runs(), stats()];
    expect(runsAfter).toBe(runsBefore + 1);
    await settle();
    expect([runs(), stats()]).toEqual([runsAfter, statsAfter]);
    expect(screen.getByRole("table")).toBe(table);
  });
});
