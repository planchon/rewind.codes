import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { createWorld, SCRIPTS } from "../mocks/fixtures";
import { NOW, renderApp, search } from "../test/app";

function groupRows(): string[] {
  return [...document.querySelectorAll("tr.group-row .mono")].map((e) => e.textContent ?? "");
}

describe("WaitingView", () => {
  it("lists the waiting runs, grouped by what they wait for, the soonest to wake first", async () => {
    const { requested } = renderApp("/waiting");
    await screen.findByRole("table");
    // Those that wake by themselves first, then the most recently started.
    expect(groupRows()).toEqual([
      'signal "approval"',
      "sleep and timers",
      "host tool · ask_user",
      "approval · ask_user",
      "tool call · github.list_pull_requests",
    ]);
    const sent = requested("/v1/runs").at(-1)?.searchParams;
    expect(sent?.get("q")).toBe("waiting:*");
    expect(sent?.get("sort")).toBe("wakes");
    // Counts by tab, from the statistics of the waiting runs.
    const tabs = screen.getByRole("tablist", { name: "What the runs wait for" });
    await waitFor(() =>
      expect(within(tabs).getByRole("tab", { name: /All/ })).toHaveTextContent("All 5"),
    );
    expect(within(tabs).getByRole("tab", { name: /Signals/ })).toHaveTextContent("Signals 1");
    expect(within(tabs).getByRole("tab", { name: /Approvals/ })).toHaveTextContent("Approvals 1");
    expect(within(tabs).getByRole("tab", { name: /Timers/ })).toHaveTextContent("Timers 1");
    expect(within(tabs).getByRole("tab", { name: /Tools/ })).toHaveTextContent("Tools 2");
  });

  it("shows the wait, since when, when it wakes and the line it is paused on", async () => {
    renderApp("/waiting");
    await screen.findByRole("table");
    const row = screen.getByText("c-42").closest("tr") as HTMLElement;
    expect(within(row).getByText('signal "approval"')).toBeInTheDocument();
    expect(within(row).getByText(/^gives up in \d+d$/)).toBeInTheDocument();
    expect(within(row).getByText("L11")).toBeInTheDocument();
    // The script is read once per digest, for its line.
    expect(
      await within(row).findByText(
        'const decision = await signal<{ close: boolean }>("approval", { timeout: "3d" });',
      ),
    ).toBeInTheDocument();
    const approval = screen.getByText("c-81").closest("tr") as HTMLElement;
    expect(within(approval).getByText("when approved")).toBeInTheDocument();
    expect(within(approval).getByText("#2 ask_user")).toBeInTheDocument();
  });

  it("reads each script once, whatever the number of its waiting runs", async () => {
    const { requested } = renderApp("/waiting");
    await screen.findByRole("table");
    await waitFor(() =>
      expect(document.querySelectorAll(".paused-line code:not(:empty)").length).toBe(5),
    );
    // Two triage runs, two onboarding runs, one digest run.
    for (const script of [SCRIPTS.TRIAGE, SCRIPTS.ONBOARDING, SCRIPTS.DIGEST])
      expect(requested(`/v1/scripts/${script.digest}`)).toHaveLength(1);
  });

  it("filters by tab, joined to the query of the URL", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/waiting?q=%40env%3Aprod");
    await screen.findByRole("table");
    await user.click(screen.getByRole("tab", { name: /Tools/ }));
    await waitFor(() => expect(search(router).get("tab")).toBe("tools"));
    await waitFor(() =>
      expect(
        requested("/v1/runs").some(
          (u) => u.searchParams.get("q") === "@env:prod waiting:call,host,deferred",
        ),
      ).toBe(true),
    );
    await waitFor(() =>
      expect(groupRows()).toEqual([
        "host tool · ask_user",
        "tool call · github.list_pull_requests",
      ]),
    );
  });

  it("opens a run at its last step", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const { router } = renderApp("/waiting", { world });
    await screen.findByRole("table");
    const row = screen.getByText("c-42").closest("tr") as HTMLElement;
    await user.click(within(row).getByText('signal "approval"'));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/runs\/[0-9a-f-]+$/));
    expect(search(router).get("step")).toBeNull();
  });
});
