import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { createWorld, type RunFixture, SCRIPTS, WORKFLOWS } from "../../mocks/fixtures";
import { shortRunId } from "../../model/format";
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

    const banner = await screen.findByRole("region", { name: "How the run ended" });
    expect(within(banner).getByRole("heading", { name: "Failed" })).toBeVisible();
    expect(within(banner).getByText("ToolError")).toBeVisible();
    expect(
      within(banner).getByText("github.close_issue: Resource not accessible by integration (403)"),
    ).toBeVisible();
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
      '"number":4106',
    );
    expect(search(router).get("entry")).not.toBeNull();
  });

  it("tells a cancellation at the deadline as a cancellation, never as a failure", async () => {
    const world = createWorld({ now: NOW });
    const run = find(world, (r) => r.script === SCRIPTS.DEV_NOTIFICATIONS);
    renderApp(`/runs/${run.id}`, { world });

    const banner = await screen.findByRole("region", { name: "How the run ended" });
    expect(
      within(banner).getByRole("heading", { name: "Cancelled at its deadline" }),
    ).toBeVisible();
    await waitFor(() =>
      expect(banner).toHaveTextContent(
        'Its deadline passed: a TTL of 11h 29m from its start. When it came, it was awaiting signal "notify" L25.',
      ),
    );
    expect(within(banner).getByText("CancelledError")).toBeVisible();
    expect(within(banner).getByText("run cancelled: its deadline has passed")).toBeVisible();
    // A stack without frames adds nothing.
    expect(within(banner).queryByText("Stack")).toBeNull();

    const bar = screen.getByRole("region", { name: "Time cursor" });
    expect(within(bar).getByRole("button", { name: /Cancelled in step \d+/ })).toBeVisible();
    expect(bar).not.toHaveTextContent(/failed/i);

    const step = screen.getByRole("region", { name: "Step" });
    expect(step).toHaveTextContent(
      "Woke up becauseIts deadline passed: the scheduler requested its cancellation",
    );
    expect(step).toHaveTextContent('the await of signal "notify" threw CancelledErrorL25');
    expect(step).toHaveTextContent(
      "ended cancelled with CancelledError run cancelled: its deadline has passed",
    );
    expect(step).toHaveTextContent(/EndedCancelled at /);
    expect(step).not.toHaveTextContent(/failed|threw CancelledError:/);
    // The wait the cancellation rejected is not still waiting.
    await waitFor(() =>
      expect(within(notesOf(25)).getByRole("button")).toHaveTextContent('signal "notify"cancelled'),
    );
  });

  it("selects the failed call from the banner", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const run = find(world, (r) => r.labels.conversation === "c-39" && r.script === SCRIPTS.TRIAGE);
    renderApp(`/runs/${run.id}`, { world });
    const banner = await screen.findByRole("region", { name: "How the run ended" });
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
    const { router } = renderApp(`/runs/${run.id}`, { world });
    const sections = await screen.findByRole("navigation", { name: "Run" });
    await waitFor(() =>
      expect(within(sections).getByRole("link", { name: /Journal/ })).toHaveTextContent(
        String(run.journal.entries.length),
      ),
    );
    await user.click(within(sections).getByRole("link", { name: /Journal/ }));
    expect(router.state.location.pathname).toBe(`/runs/${run.id}/journal`);
    expect((await screen.findAllByText("schedule_tool_call")).length).toBeGreaterThan(0);
    await user.click(within(sections).getByRole("link", { name: /Logs/ }));
    expect(screen.getAllByText("provision failed (1/3), retrying in 1m").length).toBeGreaterThan(0);
  });

  describe("workflows", () => {
    const world = createWorld({ now: NOW });
    const parent = find(world, (r) => r.script === SCRIPTS.TRIAGE_ALL);
    const children = world.runs.filter((r) => r.startedBy?.run === parent.id);

    it("lists the runs a run started, with how each ended, and reads their calls naturally", async () => {
      const user = userEvent.setup();
      const { router } = renderApp(`/runs/${parent.id}/started`, { world });
      const panel = await screen.findByRole("region", { name: "Runs it started" });
      expect(children).toHaveLength(3);
      for (const child of children) {
        expect(
          within(panel)
            .getAllByRole("link")
            .some((a) => a.getAttribute("href") === `/runs/${child.id}`),
        ).toBe(true);
      }
      expect(within(panel).getAllByText("completed")).toHaveLength(2);
      expect(within(panel).getByText("detached")).toBeInTheDocument();
      // The detached run ended after this one: its end never came back.
      expect(within(panel).getByText("not ended with this run")).toBeInTheDocument();
      expect(within(panel).getByRole("link", { name: "In Observability" })).toHaveAttribute(
        "href",
        `/observability?q=started_by%3A${parent.id}`,
      );
      const [first, second] = children as [RunFixture, RunFixture];

      // The end of a run, selected: the run it is the end of, and its status.
      await user.click(
        within(panel).getAllByRole("button", { name: /end at entry/ })[0] as HTMLElement,
      );
      const inspector = screen.getByRole("complementary", { name: "Inspector" });
      expect(await within(inspector).findByText("Result of a run")).toBeInTheDocument();
      expect(within(inspector).getByRole("link", { name: first.id })).toHaveAttribute(
        "href",
        `/runs/${first.id}`,
      );
      expect(within(inspector).getByText("completed")).toBeInTheDocument();

      // The call that started a run links to it.
      await user.click(
        within(panel).getAllByRole("button", { name: "#1 triage.start(…)" })[0] as HTMLElement,
      );
      expect(await within(inspector).findByText("#1 triage.start(…)")).toBeInTheDocument();
      expect(within(inspector).getByRole("link", { name: first.id })).toBeInTheDocument();

      // In the source, the calls read naturally.
      await router.navigate(`/runs/${parent.id}`);
      await waitFor(() =>
        expect(within(notesOf(4)).getByRole("button")).toHaveTextContent(/triage\.start\(…\)\s*×2/),
      );
      expect(within(notesOf(5)).getByRole("button")).toHaveTextContent(
        /run\.signal\("approval", …\)\s*×2/,
      );
      expect(within(notesOf(6)).getByRole("button")).toHaveTextContent(/digest\.start\(…\)/);
      expect(within(notesOf(7)).getByRole("button")).toHaveTextContent(
        /run\.result\(\)\s*×2\s*all ended/,
      );
      // In the journal, the end signals read as the ends of those runs.
      await router.navigate(`/runs/${parent.id}/journal`);
      expect(
        await screen.findByText(`end of run ${shortRunId(first.id)}: completed`),
      ).toBeInTheDocument();
      expect(
        screen.getByText(`end of run ${shortRunId(second.id)}: completed`),
      ).toBeInTheDocument();
    });

    it("shows a run of a workflow: its version, and the run that started it", async () => {
      const [child] = children as [RunFixture];
      renderApp(`/runs/${child.id}`, { world });
      const ref = `triage@${WORKFLOWS.triage.version.slice(0, 12)}`;
      expect(await screen.findByRole("link", { name: ref })).toHaveAttribute(
        "href",
        "/observability?q=workflow%3Atriage",
      );
      const started = screen.getByRole("link", { name: shortRunId(parent.id) });
      expect(started).toHaveAttribute("href", `/runs/${parent.id}`);
      expect(screen.queryByRole("region", { name: "Runs it started" })).toBeNull();
    });

    it("marks a run started detached", async () => {
      const detached = children.find((c) => c.startedBy?.detached) as RunFixture;
      renderApp(`/runs/${detached.id}`, { world });
      expect(
        await screen.findByRole("link", { name: `${shortRunId(parent.id)} · detached` }),
      ).toHaveAttribute("href", `/runs/${parent.id}`);
      expect(
        screen.getByRole("link", { name: `digest@${WORKFLOWS.digest.version.slice(0, 12)}` }),
      ).toBeInTheDocument();
    });
  });
});
