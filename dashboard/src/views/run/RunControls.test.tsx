import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { createWorld, type RunFixture, WORKFLOWS, type World } from "../../mocks/fixtures";
import { publicSummary, summarize, visibleEvents } from "../../mocks/readmodel";
import { shortRunId } from "../../model/format";
import { confirmInDialog, NOW, renderApp, signedIn } from "../../test/app";
import { changeStream, runEvents } from "../../test/streams";

/** The triage run waiting for its approval, as a run of the workflow triage, which declares it. */
function waitingRun(world: World): RunFixture {
  const fixture = world.runs.find((r) => r.labels.conversation === "c-42") as RunFixture;
  fixture.workflow = world.runs.find((r) => r.workflow?.id === WORKFLOWS.triage)?.workflow;
  return fixture;
}

/** Opens the menu of the run's controls, in its header, and picks `item`. */
async function pick(user: ReturnType<typeof userEvent.setup>, item: string) {
  await user.click(await screen.findByRole("button", { name: "Controls" }));
  await user.click(await screen.findByRole("menuitem", { name: item }));
}

function payloadBox(): HTMLTextAreaElement {
  return screen.getByRole("textbox", { name: "Payload" }) as HTMLTextAreaElement;
}

describe("the run's controls", () => {
  it("sends a declared signal: its payload from its schema, the server's refusal under it, one key per opening", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const run = waitingRun(world);
    const { callsTo } = renderApp(`/runs/${run.id}`, { world });
    // The run's events come when the test sends them.
    let streamed = visibleEvents(run, NOW).length;
    const events = runEvents(run.id, () => visibleEvents(run, NOW).slice(0, streamed));

    await pick(user, "Send approval");
    const form = await screen.findByRole("form", { name: "Send approval" });
    // The workflow declares `approval: { close: boolean }`.
    await waitFor(() => expect(payloadBox().value).toBe('{\n  "close": false\n}'));

    fireEvent.change(payloadBox(), { target: { value: '{"close": "yes"}' } });
    await user.click(within(form).getByRole("button", { name: "Send" }));
    const refusal = await within(form).findByRole("alert");
    expect(refusal).toHaveTextContent('"close" must be a boolean');
    expect(refusal).toHaveTextContent("422");
    // Sent again within the same opening: the same key.
    await user.click(within(form).getByRole("button", { name: "Send" }));
    await waitFor(() =>
      expect(callsTo(`/v1/runs/${run.id}/signals/approval`, "POST")).toHaveLength(2),
    );

    fireEvent.change(payloadBox(), { target: { value: '{"close": true}' } });
    await user.click(within(form).getByRole("button", { name: "Send" }));
    await waitFor(() => expect(screen.queryByRole("form", { name: "Send approval" })).toBeNull());
    const sent = callsTo(`/v1/runs/${run.id}/signals/approval`, "POST");
    expect(sent).toHaveLength(3);
    const keys = sent.map((c) => c.headers.get("Idempotency-Key"));
    expect(keys[0]).toBeTruthy();
    expect(new Set(keys).size).toBe(1);
    expect(await sent[2]?.body).toBe('{"close":true}');

    // "Sent", beside the menu, until the run's history says it received it.
    const note = () => screen.getByRole("status", { name: "" });
    await waitFor(() => expect(note()).toHaveTextContent("Signal approval sent"));
    const received = visibleEvents(run, NOW).slice(streamed);
    expect(received.map((e) => e.event.type)).toContain("signal_received");
    for (const event of received) events.send(event);
    streamed += received.length;
    await waitFor(() => expect(note()).toHaveTextContent(""));

    // Another opening of the form: another key.
    await pick(user, "Send a signal");
    const again = await screen.findByRole("form", { name: "Send a signal" });
    expect(within(again).getByRole("combobox", { name: "Signal" })).toHaveValue("approval");
    await user.click(within(again).getByRole("button", { name: "Send" }));
    await waitFor(() =>
      expect(callsTo(`/v1/runs/${run.id}/signals/approval`, "POST")).toHaveLength(4),
    );
    const fourth = callsTo(`/v1/runs/${run.id}/signals/approval`, "POST")[3];
    expect(fourth?.headers.get("Idempotency-Key")).not.toBe(keys[0]);
  });

  it("cancels the run after a confirmation that names it, and says so until it arrives cancelled", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const run = waitingRun(world);
    const { callsTo } = renderApp(`/runs/${run.id}`, { world });
    const changes = changeStream();

    await pick(user, "Cancel run");
    expect(
      await screen.findByRole("heading", { name: `Cancel the run ${shortRunId(run.id)}?` }),
    ).toBeVisible();
    await confirmInDialog(user);
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(callsTo(`/v1/runs/${run.id}/cancel`, "POST")).toHaveLength(1);
    expect(screen.getByRole("status", { name: "" })).toHaveTextContent("Cancelling…");
    await user.click(screen.getByRole("button", { name: "Controls" }));
    expect(await screen.findByRole("menuitem", { name: "Cancelling…" })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    await user.keyboard("{Escape}");

    // The run's summary arrives cancelled through the change stream.
    await waitFor(() => expect(changes.live).toBe(true));
    const summary = publicSummary(summarize(world, run, NOW));
    changes.run({
      ...summary,
      status: "cancelled",
      historyLen: summary.historyLen + 1,
      endedAt: new Date(NOW).toISOString(),
    });
    await waitFor(() =>
      expect(screen.getByRole("status", { name: "" })).not.toHaveTextContent("Cancelling…"),
    );
  });

  it("are not shown without runs:control", async () => {
    const world = signedIn("grace@example.net", undefined, { now: NOW });
    const run = waitingRun(world);
    const grants = world.access.grants.get("org_01ACME");
    const key = "member:user_01GRACE";
    const grant = grants?.get(key);
    if (!grants || !grant) throw new Error("no grant for Grace");
    grants.set(key, { ...grant, role: "reader" });
    renderApp(`/runs/${run.id}`, { world });
    await screen.findByRole("region", { name: "Details" });
    expect(screen.queryByRole("button", { name: "Controls" })).toBeNull();
  });
});
