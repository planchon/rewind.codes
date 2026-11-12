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
    expect(rows).toHaveLength(5);
    const forecast = rows.find((r) =>
      within(r).queryByText("// One-shot lookup started by the support agent."),
    );
    expect(forecast).toBeDefined();
    const row = within(forecast as HTMLElement);
    expect(row.getByRole("link", { name: "64" })).toHaveAttribute(
      "href",
      `/runs?script=${SCRIPTS.FORECAST.digest}`,
    );
    expect(row.getByRole("button", { name: "weather.forecast" })).toBeInTheDocument();
    expect(row.getByTitle(/\d+ completed, \d+ failed/)).toBeInTheDocument();
  });

  it("filters by tool through the API", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/scripts");
    await screen.findByRole("table");
    await user.click(screen.getAllByRole("button", { name: "crm.provision" })[0] as HTMLElement);
    await waitFor(() => expect(search(router).get("tool")).toBe("crm.provision"));
    await waitFor(() =>
      expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(2),
    );
    expect(
      requested("/v1/scripts").some((u) => u.searchParams.get("tool") === "crm.provision"),
    ).toBe(true);
  });

  it("opens a script: its source and its runs", async () => {
    renderApp(`/scripts/${SCRIPTS.ONBOARDING.digest}`);
    expect(await screen.findByText(/No account for/)).toBeInTheDocument();
    const table = await screen.findByRole("table");
    await waitFor(() => expect(within(table).getAllByRole("row")).toHaveLength(5));
  });
});
