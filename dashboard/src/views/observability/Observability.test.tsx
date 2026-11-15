import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { createWorld, SCRIPTS } from "../../mocks/fixtures";
import { NOW, renderApp, search } from "../../test/app";

function rows() {
  const table = screen.getByRole("table");
  return within(table).getAllByRole("row").slice(1);
}

function queryInput(): HTMLInputElement {
  return screen.getByRole("combobox", { name: "Query" }) as HTMLInputElement;
}

function groupRows(): string[] {
  return [...document.querySelectorAll("tr.group-row .mono")].map((e) => e.textContent ?? "");
}

describe("Observability: every run", () => {
  it("lists runs, most recently started first, with their status, labels and counts", async () => {
    renderApp("/observability");
    await screen.findByRole("table");
    expect(rows().length).toBe(50);
    expect(screen.getByRole("button", { name: "Load more" })).toBeInTheDocument();
    // The run waiting for an approval, started two hours ago.
    const triage = rows().find((r) => within(r).queryByText("c-42"));
    expect(triage).toBeDefined();
    expect(within(triage as HTMLElement).getByText("active")).toBeInTheDocument();
  });

  it("searches with the query typed, kept in the URL and sent as q", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/observability");
    await screen.findByRole("table");
    await user.type(queryInput(), "@agent:triage tool:github.close_issue{Enter}");
    await waitFor(() =>
      expect(search(router).get("q")).toBe("@agent:triage tool:github.close_issue"),
    );
    await waitFor(() =>
      expect(
        requested("/v1/runs").some(
          (u) => u.searchParams.get("q") === "@agent:triage tool:github.close_issue",
        ),
      ).toBe(true),
    );
    // Four triage runs closed issues; the corpus run and the two runs of the workflow triage too.
    await waitFor(() => expect(rows().length).toBe(7));
  });

  it("reads the filters of an older link as a query", async () => {
    const { router, requested } = renderApp(
      "/runs?status=failed&label=agent:triage&tool=github.close_issue",
    );
    await waitFor(() =>
      expect(search(router).toString()).toBe(
        "q=status%3Afailed+%40agent%3Atriage+tool%3Agithub.close_issue",
      ),
    );
    expect(queryInput().value).toBe("status:failed @agent:triage tool:github.close_issue");
    await waitFor(() => expect(rows().length).toBe(1));
    const sent = requested("/v1/runs").map((u) => u.searchParams);
    expect(sent.every((p) => p.get("status") === null && p.get("label") === null)).toBe(true);
  });

  it("reads quarantined runs and a time window from an older link", async () => {
    const { router } = renderApp("/runs?quarantined=yes&range=24h");
    await waitFor(() => expect(search(router).get("q")).toBe("quarantined:yes started:>now-24h"));
    await waitFor(() => expect(rows().length).toBe(1));
    expect(within(rows()[0] as HTMLElement).getByText("quarantined")).toBeInTheDocument();
  });

  it("lists runs on a link whose range it cannot read", async () => {
    renderApp("/observability?range=toString");
    await screen.findByRole("table");
    expect(rows().length).toBe(50);
  });

  it("suggests the fields, then the values that exist", async () => {
    const user = userEvent.setup();
    renderApp("/observability");
    await screen.findByRole("table");
    await user.click(queryInput());
    await user.type(queryInput(), "@ag");
    const options = await screen.findByRole("listbox", { name: "Suggestions" });
    expect(within(options).getByRole("option", { name: /@agent:/ })).toBeInTheDocument();
    await user.keyboard("{ArrowDown}{Enter}");
    expect(queryInput().value).toBe("@agent:");
    const values = await screen.findByRole("listbox", { name: "Suggestions" });
    expect(within(values).getByRole("option", { name: /@agent:triage/ })).toBeInTheDocument();
    await user.click(within(values).getByRole("option", { name: /@agent:triage/ }));
    expect(queryInput().value).toBe("@agent:triage ");
    await user.type(queryInput(), "sta");
    expect(
      within(await screen.findByRole("listbox", { name: "Suggestions" })).getByRole("option", {
        name: /status:/,
      }),
    ).toBeInTheDocument();
  });

  it("underlines the term the server cannot read, with its message", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/observability");
    await screen.findByRole("table");
    await user.type(queryInput(), "@env:prod stauts:failed{Enter}");
    await waitFor(() => expect(search(router).get("q")).toBe("@env:prod stauts:failed"));
    const alert = await screen.findByText(/unknown field "stauts"/);
    expect(alert.closest("[role=alert]")).toHaveTextContent("The query cannot be read");
    expect(queryInput()).toHaveAttribute("aria-invalid", "true");
    const bad = document.querySelector(".qbar-mirror .q-bad");
    expect(bad).toHaveTextContent("stauts:failed");
    expect(screen.queryByRole("table")).toBeNull();
  });

  it("adds a facet's value, excludes it, then removes it", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/observability?q=%40agent%3Asupport");
    const filters = await screen.findByRole("region", { name: "Filters" });
    const status = await within(filters).findByRole("group", { name: "Status" });
    const failed = await within(status).findByRole("button", { name: /^Status failed, \d+ runs$/ });
    await user.click(failed);
    await waitFor(() => expect(search(router).get("q")).toBe("@agent:support status:failed"));
    await waitFor(() => expect(rows().every((r) => within(r).queryByText("failed"))).toBe(true));
    await user.click(await within(status).findByRole("button", { name: /failed.*asked for/ }));
    await waitFor(() => expect(search(router).get("q")).toBe("@agent:support -status:failed"));
    // Excluded: no run has it, yet it stays, to be removed.
    await user.click(await within(status).findByRole("button", { name: /failed.*excluded/ }));
    await waitFor(() => expect(search(router).get("q")).toBe("@agent:support"));
  });

  it("adds a label from a run's labels, and shows the error of the runs that ended badly", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/observability?q=status%3Afailed");
    await screen.findByRole("table");
    await waitFor(() => expect(screen.getByRole("columnheader", { name: "Error" })).toBeVisible());
    expect(
      screen.getAllByText("Cannot read properties of undefined (reading 'text')").length,
    ).toBeGreaterThan(0);
    const chip = screen.getAllByRole("button", { name: /^env:staging/ })[0];
    await user.click(chip as HTMLElement);
    await waitFor(() => expect(search(router).get("q")).toBe("status:failed @env:staging"));
  });

  it("sorts the waiting runs by when they wake", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/observability?q=waiting%3A*");
    await screen.findByRole("table");
    await user.click(screen.getByRole("button", { name: "Wakes" }));
    await waitFor(() => expect(search(router).get("sort")).toBe("wakes"));
    await waitFor(() =>
      expect(requested("/v1/runs").some((u) => u.searchParams.get("sort") === "wakes")).toBe(true),
    );
    expect(screen.getByRole("columnheader", { name: "Wakes" })).toBeInTheDocument();
  });

  it("saves a search in the browser and lists it in the sidebar", async () => {
    const user = userEvent.setup();
    renderApp("/observability?q=status%3Afailed");
    await screen.findByRole("table");
    await user.click(screen.getByRole("button", { name: "Save" }));
    const name = screen.getByLabelText("Name of the saved search");
    await user.clear(name);
    await user.type(name, "Failed runs{Enter}");
    const saved = screen.getByRole("region", { name: "Saved searches" });
    expect(within(saved).getByRole("link", { name: /Failed runs/ })).toHaveAttribute(
      "href",
      "/observability?q=status%3Afailed",
    );
    expect(JSON.parse(window.localStorage.getItem("rewind.searches") ?? "[]")).toEqual([
      { name: "Failed runs", q: "status:failed", view: "runs" },
    ]);
    await user.click(
      within(saved).getByRole("button", { name: "Forget the saved search Failed runs" }),
    );
    expect(within(saved).queryByRole("link", { name: /Failed runs/ })).toBeNull();
  });

  it("carries the query from the Overview to Observability", async () => {
    renderApp("/overview?q=%40env%3Aprod");
    await screen.findByRole("region", { name: "Figures" });
    const nav = screen.getByRole("navigation", { name: "Views" });
    expect(within(nav).getByRole("link", { name: "Observability" })).toHaveAttribute(
      "href",
      "/observability?q=%40env%3Aprod",
    );
    expect(within(nav).getByRole("link", { name: "Scripts" })).toHaveAttribute("href", "/scripts");
  });

  it("opens the views of the same query, each with its count", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/observability?q=%40env%3Aprod");
    await screen.findByRole("table");
    await user.click(screen.getByRole("button", { name: "View: All runs" }));
    const waiting = await screen.findByRole("menuitem", { name: /Waiting/ });
    expect(waiting).toHaveAttribute("href", "/observability/waiting?q=%40env%3Aprod");
    await user.click(waiting);
    await waitFor(() => expect(router.state.location.pathname).toBe("/observability/waiting"));
    expect(await screen.findByRole("heading", { name: "Waiting" })).toBeInTheDocument();
  });

  it("shows the views again with back, without leaving the page", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/observability");
    await screen.findByRole("table");
    const views = screen.getByRole("navigation", { name: "Views", hidden: true });
    expect(views).toHaveAttribute("inert");
    await user.click(screen.getByRole("button", { name: "Show the views (Observability)" }));
    expect(views).not.toHaveAttribute("inert");
    expect(router.state.location.pathname).toBe("/observability");
    // Observability, the current view, opens its own sidebar again.
    await user.click(within(views).getByRole("link", { name: "Observability" }));
    expect(views).toHaveAttribute("inert");
  });

  it("reads an older link: the runs, the waiting and the failures, a namespace filter", async () => {
    const { router } = renderApp("/runs?ns=bob&q=%40env%3Aprod");
    await waitFor(() => expect(router.state.location.pathname).toBe("/observability"));
    expect(search(router).get("q")).toBe("@env:prod namespace:bob");
    expect(search(router).get("ns")).toBeNull();
    await router.navigate("/waiting?tab=tools");
    await waitFor(() => expect(router.state.location.pathname).toBe("/observability/waiting"));
    expect(search(router).get("tab")).toBe("tools");
    await router.navigate("/failures?window=7d");
    await waitFor(() => expect(router.state.location.pathname).toBe("/observability/failures"));
    expect(search(router).get("window")).toBe("7d");
  });

  it("loads the next page with the cursor", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp("/observability");
    await screen.findByRole("table");
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(rows().length).toBeGreaterThan(50));
    expect(requested("/v1/runs").some((u) => u.searchParams.get("cursor") === "50")).toBe(true);
  });
});

describe("Observability: the waiting runs", () => {
  it("lists the waiting runs, grouped by what they wait for, the soonest to wake first", async () => {
    const { requested } = renderApp("/observability/waiting");
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
    renderApp("/observability/waiting");
    await screen.findByRole("table");
    const row = within(screen.getByRole("table")).getByText("c-42").closest("tr") as HTMLElement;
    expect(within(row).getByText('signal "approval"')).toBeInTheDocument();
    expect(within(row).getByText(/^gives up in \d+d$/)).toBeInTheDocument();
    expect(within(row).getByText("L11")).toBeInTheDocument();
    // The script is read once per digest, for its line.
    expect(
      await within(row).findByText(
        'const decision = await signal<{ close: boolean }>("approval", { timeout: "3d" });',
      ),
    ).toBeInTheDocument();
    const approval = within(screen.getByRole("table"))
      .getByText("c-81")
      .closest("tr") as HTMLElement;
    expect(within(approval).getByText("when approved")).toBeInTheDocument();
    expect(within(approval).getByText("#2 ask_user")).toBeInTheDocument();
  });

  it("reads each script once, whatever the number of its waiting runs", async () => {
    const { requested } = renderApp("/observability/waiting");
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
    const { router, requested } = renderApp("/observability/waiting?q=%40env%3Aprod");
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
    const { router } = renderApp("/observability/waiting", { world });
    await screen.findByRole("table");
    const row = within(screen.getByRole("table")).getByText("c-42").closest("tr") as HTMLElement;
    await user.click(within(row).getByText('signal "approval"'));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/runs\/[0-9a-f-]+$/));
    expect(search(router).get("step")).toBeNull();
  });
});

describe("Observability: the failures", () => {
  it("groups the runs that ended badly in the window by signature", async () => {
    const { requested } = renderApp("/observability/failures?window=7d");
    const table = await screen.findByRole("table");
    const rows = within(table).getAllByRole("row").slice(1);
    // The forecast timeouts, its refused connections, the TypeError, the 403, the cancellation,
    // and the abort: across scripts, not per script.
    expect(rows.length).toBeGreaterThanOrEqual(5);
    const typeError = rows.find((r) =>
      within(r).queryByText("TypeError: Cannot read properties of undefined (reading 'text')"),
    ) as HTMLElement;
    expect(typeError).toBeDefined();
    expect(within(typeError).getByText("L16")).toBeInTheDocument();
    expect(
      within(typeError).getByRole("img", { name: /Runs of TypeError by 6h/ }),
    ).toBeInTheDocument();
    const timeout = rows.find((r) =>
      within(r).queryByText("ToolError: weather.forecast: no answer within <n>s"),
    ) as HTMLElement;
    expect(timeout).toBeDefined();
    expect(within(timeout).getByText("L5")).toBeInTheDocument();
    expect(within(timeout).getByRole("button", { name: /agent:support/ })).toBeInTheDocument();
    const sent = requested("/v1/runs/failures").at(-1)?.searchParams;
    expect(sent?.get("bucket")).toBe("6h");
    expect(Date.now() - Date.parse(sent?.get("since") ?? "")).toBeGreaterThan(
      7 * 86_400_000 - 60_000,
    );
    expect(screen.getByText(/runs, \d+ signatures, ended in the last 7 days/)).toBeInTheDocument();
  });

  it("opens a group's runs, and its latest run on the step that failed", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const digest = world.runs.find(
      (r) => r.script === SCRIPTS.DIGEST && r.labels.schedule === "adhoc" && r.errorAt,
    );
    const { router } = renderApp("/observability/failures", { world });
    const table = await screen.findByRole("table");
    const row = within(table)
      .getByText("TypeError: Cannot read properties of undefined (reading 'text')")
      .closest("tr") as HTMLElement;
    expect(within(row).getByRole("link", { name: /…/ })).toHaveAttribute(
      "href",
      `/runs/${digest?.id}`,
    );
    await user.click(within(row).getByRole("link", { name: "1" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/observability"));
    expect(search(router).get("q")).toMatch(/^fingerprint:[0-9a-f]{16} ended:>now-24h$/);
    const runs = await screen.findByRole("table");
    await waitFor(() => expect(within(runs).getAllByRole("row")).toHaveLength(2));
  });

  it("joins a label to the query", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/observability/failures?window=7d");
    const table = await screen.findByRole("table");
    const label = within(table).getAllByRole("button", { name: /agent:support/ })[0];
    await user.click(label as HTMLElement);
    await waitFor(() => expect(search(router).get("q")).toBe("@agent:support"));
    expect(search(router).get("window")).toBe("7d");
  });
});
