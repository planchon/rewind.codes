import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { createWorld, type RunFixture, SCRIPTS } from "../../mocks/fixtures";
import { NOW, renderApp, search } from "../../test/app";

function find(world: ReturnType<typeof createWorld>, test: (r: RunFixture) => boolean): RunFixture {
  const run = world.runs.find(test);
  if (!run) throw new Error("no such fixture");
  return run;
}

function notesOf(line: number): HTMLElement {
  const rows = document.querySelectorAll<HTMLElement>(".code-notes .code-row");
  const row = rows[line - 1];
  if (!row) throw new Error(`no line ${line}`);
  return row;
}

describe("RunView", () => {
  it("shows a failed run: the error, its line, and each call next to the line that made it", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const run = find(world, (r) => r.labels.conversation === "c-39" && r.script === SCRIPTS.TRIAGE);
    const { router } = renderApp(`/runs/${run.id}`, { world });

    expect(await screen.findByRole("region", { name: "Error" })).toHaveTextContent(
      "ToolError: github.close_issue: Resource not accessible by integration (403)",
    );
    // The journal arrives through the event stream.
    await waitFor(() =>
      expect(within(notesOf(14)).getByRole("button")).toHaveTextContent(
        /github\.close_issue\s*×5\s*1 failed/,
      ),
    );
    expect(within(notesOf(5)).getByRole("button")).toHaveTextContent(/github\.list_issues/);
    expect(within(notesOf(7)).getByRole("button")).toHaveTextContent(
      "5 of 12 open issues are stale",
    );
    expect(within(notesOf(11)).getByRole("button")).toHaveTextContent(/signal "approval".*mailbox/);
    expect(document.querySelector(".code-text .code-row.mark-error")).toHaveAttribute(
      "data-line",
      "14",
    );

    // Several calls on a line: the line's list, then one call.
    await user.click(within(notesOf(14)).getByRole("button"));
    await waitFor(() => expect(search(router).get("line")).toBe("14"));
    const inspector = screen.getByRole("complementary", { name: "Inspector" });
    expect(within(inspector).getByText("script.ts:14")).toBeInTheDocument();
    await user.click(within(inspector).getByText("#6 github.close_issue"));
    await waitFor(() =>
      expect(
        within(inspector).getByText(/Resource not accessible by integration \(403\)/),
      ).toBeInTheDocument(),
    );
    expect(within(inspector).getByRole("region", { name: "args" })).toHaveTextContent(
      '"number": 4106',
    );
    expect(search(router).get("entry")).not.toBeNull();
  });

  it("selects the failed call from the banner", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const run = find(world, (r) => r.labels.conversation === "c-39" && r.script === SCRIPTS.TRIAGE);
    renderApp(`/runs/${run.id}`, { world });
    const banner = await screen.findByRole("region", { name: "Error" });
    await waitFor(() =>
      expect(within(banner).getByRole("button", { name: /thrown by #6/ })).toBeEnabled(),
    );
    await user.click(within(banner).getByRole("button", { name: /thrown by #6/ }));
    const inspector = screen.getByRole("complementary", { name: "Inspector" });
    expect(await within(inspector).findByText("#6 github.close_issue")).toBeInTheDocument();
  });

  it("loads a payload above the inline limit only when asked", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const run = find(world, (r) => r.labels.conversation === "c-42");
    const { requested } = renderApp(`/runs/${run.id}?entry=1`, { world });

    expect(await screen.findByRole("region", { name: "Waiting for" })).toHaveTextContent(
      /signal "approval", gives up in 2d/,
    );
    const inspector = screen.getByRole("complementary", { name: "Inspector" });
    const result = await within(inspector).findByRole("region", { name: "result" });
    const completed = run.journal.entries[2]?.event;
    if (completed?.type !== "tool_call_completed") throw new Error("entry 2 is not the completion");
    const path = `/v1/payloads/${completed.result.digest}`;
    // The entry streamed without the value: only its preview is shown.
    expect(completed.result.value).toBeUndefined();
    expect(requested(path)).toHaveLength(0);
    await user.click(within(result).getByRole("button", { name: /Load the value \(\d+ KB\)/ }));
    expect(await within(result).findByText(/Issue 4100: flaky test in ci/)).toBeInTheDocument();
    expect(requested(path)).toHaveLength(1);
  });

  it("follows a run live: new entries appear, then its end", async () => {
    let clock = NOW;
    const world = createWorld({ now: NOW, live: true });
    // The live run is the last of the world.
    const run = world.runs[world.runs.length - 1] as RunFixture;
    renderApp(`/runs/${run.id}`, { world, clock: () => clock });

    expect(await screen.findByText("live")).toBeInTheDocument();
    await waitFor(() => expect(screen.getAllByText("active").length).toBeGreaterThan(0));
    expect(document.querySelector(".code-notes .code-row:nth-child(11) .note")).toBeNull();

    clock = NOW + 120_000;
    await waitFor(
      () => expect(within(notesOf(11)).getByRole("button")).toHaveTextContent("llm.complete"),
      { timeout: 4_000 },
    );
    await waitFor(() => expect(screen.getByText("complete journal")).toBeInTheDocument());
    await waitFor(() => expect(screen.getAllByText("completed").length).toBeGreaterThan(0), {
      timeout: 4_000,
    });
    expect(within(notesOf(6)).getByRole("button")).toHaveTextContent(
      /github\.list_pull_requests\s*×6/,
    );
  });

  it("compresses a ten-day sleep on the timeline, and keeps every span", async () => {
    const world = createWorld({ now: NOW });
    const run = find(world, (r) => r.labels.conversation === "c-41");
    renderApp(`/runs/${run.id}`, { world });
    const timeline = await screen.findByRole("region", { name: "Timeline" });
    await waitFor(() => expect(within(timeline).getByText("⋯10d")).toBeInTheDocument());
    expect(within(timeline).getByText("⋯2h 14m")).toBeInTheDocument();
    expect(within(timeline).getByText("8 spans")).toBeInTheDocument();
    expect(
      within(timeline).getByRole("button", { name: "#7 sleep 10d: fired" }),
    ).toBeInTheDocument();
  });

  it("says when the run does not exist", async () => {
    renderApp("/runs/01000000-0000-7000-a000-000000000000");
    expect(await screen.findByText("No such run")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Back to the runs" })).toBeInTheDocument();
  });

  it("shows the journal and the logs", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const run = find(world, (r) => r.labels.conversation === "c-77");
    renderApp(`/runs/${run.id}`, { world });
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: /Journal/ })).toHaveTextContent(
        String(run.journal.entries.length),
      ),
    );
    expect(screen.getAllByText("schedule_tool_call").length).toBeGreaterThan(0);
    await user.click(screen.getByRole("tab", { name: /Logs/ }));
    expect(screen.getAllByText("provision failed (1/3), retrying in 1m").length).toBeGreaterThan(0);
  });
});
