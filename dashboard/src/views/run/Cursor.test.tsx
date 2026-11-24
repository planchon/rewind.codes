import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { CODES, createWorld, type RunFixture } from "../../mocks/fixtures";
import { NOW, renderApp, search } from "../../test/app";

/** The run of the e2e corpus's triage_workflow: 18 entries, 8 steps. */
function corpusRun(): { world: ReturnType<typeof createWorld>; run: RunFixture } {
  const world = createWorld({ now: NOW });
  const run = world.runs.find((r) => r.code === CODES.TRIAGE_WORKFLOW);
  if (!run) throw new Error("no corpus run");
  return { world, run };
}

function bar(): HTMLElement {
  return screen.getByRole("region", { name: "Time cursor" });
}

/** The mark of a line in the gutter. */
function markOf(line: number): string {
  const row = document.querySelectorAll(".code-gutter .code-row")[line - 1];
  return row?.querySelector(".mk")?.textContent ?? "";
}

function notesOf(line: number): HTMLElement {
  const row = document.querySelectorAll<HTMLElement>(".code-notes .code-row")[line - 1];
  if (!row) throw new Error(`no line ${line}`);
  return row;
}

/** A hover target of a line: a call, a variable bound to one, or the input. */
async function target(line: number, kind: "call" | "binding" | "input"): Promise<HTMLElement> {
  let el: HTMLElement | null = null;
  await waitFor(() => {
    el = document.querySelector<HTMLElement>(`.code-text [data-line="${line}"] .hv-${kind}`);
    if (!el) throw new Error(`no ${kind} on line ${line}`);
  });
  return el as unknown as HTMLElement;
}

describe("the time cursor", () => {
  it("steps through the history, skipping the steps that issue nothing", async () => {
    const user = userEvent.setup();
    const { world, run } = corpusRun();
    const { router } = renderApp(`/runs/${run.id}`, { world });
    await waitFor(() => expect(bar()).toHaveTextContent("Step 8 of 8"));
    expect(search(router).get("step")).toBeNull();

    await user.click(within(bar()).getByRole("button", { name: /Step back/ }));
    await waitFor(() => expect(search(router).get("step")).toBe("14"));
    expect(bar()).toHaveTextContent("Step 7 of 8");
    await user.keyboard("{ArrowLeft}");
    await waitFor(() => expect(search(router).get("step")).toBe("12"));
    // Step 5 delivers #4 while the script still waits for #5: skipped.
    await user.keyboard("{ArrowLeft}");
    await waitFor(() => expect(search(router).get("step")).toBe("7"));
    expect(bar()).toHaveTextContent("Step 4 of 8");
    await user.click(within(bar()).getByRole("checkbox", { name: "every step" }));
    await user.keyboard("{ArrowRight}");
    await waitFor(() => expect(search(router).get("step")).toBe("11"));
    await user.keyboard("{Home}");
    await waitFor(() => expect(search(router).get("step")).toBe("0"));
    expect(within(bar()).getByRole("button", { name: "First step" })).toBeDisabled();
    // The last step is the head: the URL names none.
    await user.keyboard("{End}");
    await waitFor(() => expect(search(router).get("step")).toBeNull());
    expect(bar()).toHaveTextContent("Step 8 of 8");
  });

  it("marks each line at a step, and shows the notes and the timeline as they were", async () => {
    const { world, run } = corpusRun();
    renderApp(`/runs/${run.id}?step=7`, { world });
    await waitFor(() => expect(bar()).toHaveTextContent("Step 4 of 8"));
    await waitFor(() => expect(markOf(10)).toBe("▸"));
    expect(markOf(13)).toBe("①");
    expect(markOf(5)).toBe("✓");
    expect(markOf(8)).toBe("✓");
    expect(markOf(15)).toBe("");
    expect(within(notesOf(13)).getByRole("button")).toHaveTextContent(
      /close_issue\s*×2.*2 in flight/,
    );
    // The sleep of line 15 has not started at this step.
    expect(within(notesOf(15)).queryByRole("button")).toBeNull();

    const step = screen.getByRole("region", { name: "Step" });
    expect(step).toHaveTextContent(
      /Woke up because\s*Signal "approval" arrived with \{"close":true\}/,
    );
    expect(step).toHaveTextContent(/Then it.*cancelled timer #3.*called\s*#4 github\.close_issue/);
    expect(step).toHaveTextContent(/Now paused on.*#4 github\.close_issue, in flight.*#5/);

    const timeline = screen.getByRole("region", { name: "Timeline" });
    expect(within(timeline).getByRole("slider", { name: /Time cursor/ })).toHaveAttribute(
      "aria-valuenow",
      "4",
    );
    expect(timeline.querySelectorAll(".tl-row.future").length).toBe(2);
  });

  it("looks as it does without a cursor at the last step", async () => {
    const { world, run } = corpusRun();
    renderApp(`/runs/${run.id}`, { world });
    await waitFor(() =>
      expect(within(notesOf(15)).getByRole("button")).toHaveTextContent("sleep 10d"),
    );
    expect(document.querySelectorAll(".code-gutter .mk").length).toBe(0);
    expect(document.querySelectorAll(".tl-row.future").length).toBe(0);
  });

  it("shows a variable's value as known at the cursor, and when it is not yet", async () => {
    const user = userEvent.setup();
    const { world, run } = corpusRun();
    renderApp(`/runs/${run.id}?step=2`, { world });
    await user.hover(await target(6, "binding"));
    const pop = await screen.findByRole("tooltip");
    expect(pop).toHaveTextContent("= the result of #1 github.list_issues");
    expect(pop).toHaveTextContent('"updated_at":"2025-10-01T00:00:00Z"');

    await user.unhover(await target(6, "binding"));
    await user.hover(await target(11, "binding"));
    await waitFor(() =>
      expect(screen.getByRole("tooltip")).toHaveTextContent(
        "not assigned yet: it is assigned in step 4",
      ),
    );

    await user.hover(await target(5, "input"));
    await waitFor(() =>
      expect(screen.getByRole("tooltip")).toHaveTextContent(/The run's input.*"repo":"acme\/api"/),
    );
  });

  it("shows a call's arguments and result at the cursor, every call of a fan-out", async () => {
    const user = userEvent.setup();
    const { world, run } = corpusRun();
    renderApp(`/runs/${run.id}?step=2`, { world });
    await user.hover(await target(13, "call"));
    const before = await screen.findByRole("tooltip");
    expect(before).toHaveTextContent("2 calls from this line");
    expect(before).toHaveTextContent("Not called yet: it is called in step 4.");
    await user.unhover(await target(13, "call"));
    await user.keyboard("{ArrowRight}{ArrowRight}");
    await user.hover(await target(13, "call"));
    await waitFor(() =>
      expect(screen.getByRole("tooltip")).toHaveTextContent("In flight since step 4."),
    );
    const pop = screen.getByRole("tooltip");
    expect(pop).toHaveTextContent('"number":3');

    await user.unhover(await target(13, "call"));
    await user.keyboard("{End}");
    await user.hover(await target(13, "call"));
    await waitFor(() =>
      expect(screen.getByRole("tooltip")).toHaveTextContent(/returned.*"repo":"acme\/api"/),
    );
  });

  it("pins a value as a watch that follows the cursor", async () => {
    const user = userEvent.setup();
    const { world, run } = corpusRun();
    renderApp(`/runs/${run.id}?step=2`, { world });
    await user.click(await target(6, "binding"));
    const watches = await screen.findByRole("list", { name: "Watches" });
    expect(within(watches).getByText("issues")).toBeInTheDocument();
    expect(watches).toHaveTextContent('[{"number":1,"updated_at":"2025-10-01T00:00:00Z"}');
    await user.keyboard("{Home}");
    await waitFor(() =>
      expect(screen.getByRole("list", { name: "Watches" })).toHaveTextContent(
        "not assigned yet: the await is still open, since step 1",
      ),
    );
    await user.click(screen.getByRole("button", { name: "Stop watching issues" }));
    expect(screen.queryByRole("list", { name: "Watches" })).toBeNull();
  });

  it("marks the step a failed run failed in, and its error line", async () => {
    const world = createWorld({ now: NOW });
    const run = world.runs.find(
      (r) => r.labels.conversation === "c-39" && r.code === CODES.TRIAGE,
    ) as RunFixture;
    renderApp(`/runs/${run.id}`, { world });
    await waitFor(() =>
      expect(within(bar()).getByRole("button", { name: /Failed in step \d+/ })).toBeVisible(),
    );
    await waitFor(() =>
      expect(document.querySelector(".code-text .code-row.mark-error")).toHaveAttribute(
        "data-line",
        "14",
      ),
    );
  });

  it("follows the head of a live run until the operator steps back", async () => {
    const user = userEvent.setup();
    let clock = NOW;
    const world = createWorld({ now: NOW, live: true });
    const run = world.runs[world.runs.length - 1] as RunFixture;
    const { router } = renderApp(`/runs/${run.id}`, { world, clock: () => clock });
    await waitFor(() => expect(bar()).toHaveTextContent(/following the head/));
    const first = within(bar())
      .getByText(/^Step \d+ of \d+$/)
      .textContent?.match(/Step (\d+) of (\d+)/);
    expect(first?.[1]).toBe(first?.[2]);

    await user.click(within(bar()).getByRole("button", { name: /Step back/ }));
    await waitFor(() => expect(search(router).get("step")).not.toBeNull());
    const pinned = search(router).get("step");
    clock = NOW + 120_000;
    await waitFor(
      () =>
        expect(
          within(bar()).getByRole("button", { name: /Completed in step \d+/ }),
        ).toBeInTheDocument(),
      { timeout: 4_000 },
    );
    // New entries arrived: the cursor stays where the operator put it.
    expect(search(router).get("step")).toBe(pinned);
    await user.click(within(bar()).getByRole("button", { name: "Last step" }));
    await waitFor(() => expect(search(router).get("step")).toBeNull());
    expect(within(bar()).getByRole("button", { name: /Completed in step \d+/ })).toHaveAttribute(
      "aria-current",
      "step",
    );
  });
});
