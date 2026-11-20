import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { SCRIPTS } from "../mocks/fixtures";
import { renderApp, search } from "../test/app";

describe("ScriptsView", () => {
  it("lists every distinct script with its runs by outcome and its tools", async () => {
    renderApp("/scripts");
    const table = await screen.findByRole("table");
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(9);
    const forecast = rows.find((r) =>
      within(r).queryByText("// One-shot lookup started by the support agent."),
    );
    expect(forecast).toBeDefined();
    const row = within(forecast as HTMLElement);
    expect(row.getByRole("link", { name: "64" })).toHaveAttribute(
      "href",
      `/observability?q=script%3A${SCRIPTS.FORECAST.digest}`,
    );
    expect(row.getByRole("button", { name: "weather.forecast" })).toBeInTheDocument();
    expect(row.getByTitle(/\d+ completed, \d+ failed/)).toBeInTheDocument();
  });

  it("filters by tool through the API, from the query bar", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/scripts");
    await screen.findByRole("table");
    await user.click(screen.getAllByRole("button", { name: "crm.provision" })[0] as HTMLElement);
    await waitFor(() => expect(search(router).get("q")).toBe("tool:crm.provision"));
    expect(screen.getByRole("combobox", { name: "Query" })).toHaveValue("tool:crm.provision");
    await waitFor(() =>
      expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(2),
    );
    expect(
      requested("/v1/scripts").some((u) => u.searchParams.get("tool") === "crm.provision"),
    ).toBe(true);
  });

  it("searches by label, and underlines what the list cannot read", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/scripts?label=agent:triage");
    // An older link's filter is read as the query it means.
    await waitFor(() => expect(search(router).get("q")).toBe("@agent:triage"));
    expect(search(router).get("label")).toBeNull();
    await waitFor(() =>
      expect(
        requested("/v1/scripts").some((u) => u.searchParams.getAll("label")[0] === "agent:triage"),
      ).toBe(true),
    );

    const bar = screen.getByRole("combobox", { name: "Query" });
    await user.clear(bar);
    await user.type(bar, "status:failed{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent(/searched by namespace/);
    expect(bar).toHaveAttribute("aria-invalid", "true");
    expect(screen.queryByRole("table")).toBeNull();
  });

  it("narrows to one namespace with Rewind-Namespace, and its links to the runs too", async () => {
    const { callsTo } = renderApp("/scripts?q=namespace:chen");
    const table = await screen.findByRole("table");
    await waitFor(() =>
      expect(callsTo("/v1/scripts").at(-1)?.headers.get("Rewind-Namespace")).toBe("chen"),
    );
    // chen runs the digest agent alone.
    await waitFor(() => expect(within(table).getAllByRole("row")).toHaveLength(2));
    // Its display name, for its id.
    expect(screen.getByText(/The sources the runs of/)).toHaveTextContent("Chen Logistics");
    const runs = within(table).getAllByTitle("Its runs")[0] as HTMLElement;
    expect(new URL(runs.getAttribute("href") ?? "", "http://x").searchParams.get("q")).toMatch(
      /^script:\S+ namespace:chen$/,
    );
  });

  it("opens a script: its source and its runs", async () => {
    renderApp(`/scripts/${SCRIPTS.ONBOARDING.digest}`);
    expect(await screen.findByText(/No account for/)).toBeInTheDocument();
    const table = await screen.findByRole("table");
    // Its header and its five runs.
    await waitFor(() => expect(within(table).getAllByRole("row")).toHaveLength(6));
  });
});
