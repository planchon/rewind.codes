import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { renderApp, search } from "../test/app";

function rows() {
  const table = screen.getByRole("table");
  return within(table).getAllByRole("row").slice(1);
}

function queryInput(): HTMLInputElement {
  return screen.getByRole("combobox", { name: "Query" }) as HTMLInputElement;
}

describe("RunsView", () => {
  it("lists runs, most recently started first, with their status, labels and counts", async () => {
    renderApp("/runs");
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
    const { router, requested } = renderApp("/runs");
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
    renderApp("/runs?range=toString");
    await screen.findByRole("table");
    expect(rows().length).toBe(50);
  });

  it("suggests the fields, then the values that exist", async () => {
    const user = userEvent.setup();
    renderApp("/runs");
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
    const { router } = renderApp("/runs");
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
    const { router } = renderApp("/runs?q=%40agent%3Asupport");
    const facets = await screen.findByRole("complementary", { name: "Facets" });
    const status = await within(facets).findByRole("region", { name: "Status" });
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
    const { router } = renderApp("/runs?q=status%3Afailed");
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
    const { router, requested } = renderApp("/runs?q=waiting%3A*");
    await screen.findByRole("table");
    await user.click(screen.getByRole("button", { name: "Wakes" }));
    await waitFor(() => expect(search(router).get("sort")).toBe("wakes"));
    await waitFor(() =>
      expect(requested("/v1/runs").some((u) => u.searchParams.get("sort") === "wakes")).toBe(true),
    );
    expect(screen.getByRole("columnheader", { name: "Wakes" })).toBeInTheDocument();
  });

  it("saves a search in the browser and lists it in the navigation", async () => {
    const user = userEvent.setup();
    renderApp("/runs?q=status%3Afailed");
    await screen.findByRole("table");
    await user.click(screen.getByRole("button", { name: "Save" }));
    const name = screen.getByLabelText("Name of the saved search");
    await user.clear(name);
    await user.type(name, "Failed runs{Enter}");
    const nav = screen.getByRole("navigation", { name: "Views" });
    const saved = within(nav).getByRole("link", { name: /Failed runs/ });
    expect(saved).toHaveAttribute("href", "/runs?q=status%3Afailed");
    expect(JSON.parse(window.localStorage.getItem("rewind.searches") ?? "[]")).toEqual([
      { name: "Failed runs", q: "status:failed", view: "runs" },
    ]);
    await user.click(
      within(nav).getByRole("button", { name: "Forget the saved search Failed runs" }),
    );
    expect(within(nav).queryByRole("link", { name: /Failed runs/ })).toBeNull();
  });

  it("carries the query to the other views in the navigation", async () => {
    renderApp("/runs?q=%40env%3Aprod");
    await screen.findByRole("table");
    const nav = screen.getByRole("navigation", { name: "Views" });
    expect(within(nav).getByRole("link", { name: "Waiting" })).toHaveAttribute(
      "href",
      "/waiting?q=%40env%3Aprod",
    );
    expect(within(nav).getByRole("link", { name: "Scripts" })).toHaveAttribute("href", "/scripts");
  });

  it("loads the next page with the cursor", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp("/runs");
    await screen.findByRole("table");
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(rows().length).toBeGreaterThan(50));
    expect(requested("/v1/runs").some((u) => u.searchParams.get("cursor") === "50")).toBe(true);
  });
});
