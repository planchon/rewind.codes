import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { createWorld, SCRIPTS } from "../mocks/fixtures";
import { NOW, renderApp, search } from "../test/app";

describe("FailuresView", () => {
  it("groups the runs that ended badly in the window by signature", async () => {
    const { requested } = renderApp("/failures?window=7d");
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
    const { router } = renderApp("/failures", { world });
    const table = await screen.findByRole("table");
    const row = within(table)
      .getByText("TypeError: Cannot read properties of undefined (reading 'text')")
      .closest("tr") as HTMLElement;
    expect(within(row).getByRole("link", { name: /…/ })).toHaveAttribute(
      "href",
      `/runs/${digest?.id}`,
    );
    await user.click(within(row).getByRole("link", { name: "1" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/runs"));
    expect(search(router).get("q")).toMatch(/^fingerprint:[0-9a-f]{16} ended:>now-24h$/);
    const runs = await screen.findByRole("table");
    await waitFor(() => expect(within(runs).getAllByRole("row")).toHaveLength(2));
  });

  it("joins a label to the query", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/failures?window=7d");
    const table = await screen.findByRole("table");
    const label = within(table).getAllByRole("button", { name: /agent:support/ })[0];
    await user.click(label as HTMLElement);
    await waitFor(() => expect(search(router).get("q")).toBe("@agent:support"));
    expect(search(router).get("window")).toBe("7d");
  });
});
