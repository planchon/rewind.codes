import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { SCRIPTS } from "../mocks/fixtures";
import { scriptDoc } from "../mocks/handles";
import { renderApp, search } from "../test/app";

describe("ScriptsView", () => {
  it("lists every distinct script of the window with its runs by outcome and its tools", async () => {
    renderApp("/scripts?window=30d");
    const table = await screen.findByRole("table");
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(9);
    expect(within(table).queryByRole("columnheader", { name: "Labels" })).toBeNull();
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

  it("counts the window's runs, the last 24 hours by default", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/scripts");
    const table = await screen.findByRole("table");
    await waitFor(() => expect(within(table).getAllByRole("row")).toHaveLength(7));
    const since = (u: URL) => Date.parse(u.searchParams.get("since") ?? "");
    const day = requested("/v1/scripts").at(-1) as URL;
    expect(Date.now() - since(day)).toBeGreaterThanOrEqual(86_400_000 - 60_000);
    expect(Date.now() - since(day)).toBeLessThan(86_400_000 + 60_000);

    await user.click(screen.getByRole("button", { name: "30d" }));
    await waitFor(() => expect(search(router).get("window")).toBe("30d"));
    await waitFor(() => expect(within(table).getAllByRole("row")).toHaveLength(10));
    const month = requested("/v1/scripts").at(-1) as URL;
    expect(Date.now() - since(month)).toBeGreaterThanOrEqual(30 * 86_400_000 - 60_000);
  });

  it("underlines a label, which the counters know nothing of, and what the list cannot read", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/scripts?label=agent:triage");
    // An older link's filter is read as the query it means, which the list cannot read.
    await waitFor(() => expect(search(router).get("q")).toBe("@agent:triage"));
    expect(search(router).get("label")).toBeNull();
    expect(await screen.findByRole("alert")).toHaveTextContent(/know no labels/);
    expect(requested("/v1/scripts")).toHaveLength(0);

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

describe("ScriptView", () => {
  it("lists the script's runs in the table forced to it; an older status link is a filter", async () => {
    const user = userEvent.setup();
    const digest = SCRIPTS.FORECAST.digest;
    const { router, requested } = renderApp(`/scripts/${digest}?status=completed`);
    await waitFor(() => expect(search(router).get("q")).toBe("status:completed"));
    const section = await screen.findByRole("region", { name: "Runs of this script" });
    await waitFor(() => expect(within(section).getAllByRole("row").length).toBeGreaterThan(1));
    expect(
      requested("/v1/runs").some(
        (u) => u.searchParams.get("q") === `script:${digest} status:completed`,
      ),
    ).toBe(true);
    // No script column: the script is the page's.
    expect(within(section).queryByRole("columnheader", { name: "Script" })).toBeNull();
    const chips = within(section).getByRole("group", { name: "Filters" });
    // The chip names the script by its title, the first line of the comment at its top.
    const title = scriptDoc(SCRIPTS.FORECAST.source)?.title ?? digest.slice(0, 12);
    await waitFor(() => expect(chips).toHaveTextContent(`Script ${title}`));

    await user.click(within(chips).getByRole("button", { name: "Clear the filters" }));
    await waitFor(() => expect(search(router).get("q")).toBeNull());
    await waitFor(() =>
      expect(requested("/v1/runs").at(-1)?.searchParams.get("q")).toBe(`script:${digest}`),
    );
  });

  it("gives a script a title, which names it, then takes it back for its comment's", async () => {
    const user = userEvent.setup();
    const digest = SCRIPTS.FORECAST.digest;
    const comment = scriptDoc(SCRIPTS.FORECAST.source)?.title ?? "";
    const { callsTo } = renderApp(`/scripts/${digest}`);
    const heading = await screen.findByRole("heading", { level: 1 });
    await waitFor(() => expect(heading).toHaveTextContent(comment));

    await user.click(screen.getByRole("button", { name: "Edit the title" }));
    const form = await screen.findByRole("form", { name: "Title of the script" });
    const title = within(form).getByLabelText("Title");
    await user.clear(title);
    await user.type(title, "  Weather for a trip  ");
    await user.type(within(form).getByLabelText("Description"), "Looks up the forecast.");
    await user.click(within(form).getByRole("button", { name: "Save the title" }));
    await waitFor(() => expect(heading).toHaveTextContent("Weather for a trip"));
    const patch = callsTo(`/v1/scripts/${digest}`, "PATCH")[0];
    expect(JSON.parse((await patch?.body) ?? "")).toEqual({
      title: "Weather for a trip",
      description: "Looks up the forecast.",
    });

    await user.click(screen.getByRole("button", { name: "Edit the title" }));
    const again = await screen.findByRole("form", { name: "Title of the script" });
    await user.click(
      within(again).getByRole("button", { name: "Remove this title: the comment's shows again" }),
    );
    await waitFor(() => expect(heading).toHaveTextContent(comment));
  });
});
