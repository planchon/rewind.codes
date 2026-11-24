import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { ActionStats } from "../model/types";
import { renderApp, search } from "../test/app";
import { groupActions, groupOf, logScale, logTicks } from "./ActionsView";

function stats(action: string, p50: number, max: number): ActionStats {
  return {
    action,
    calls: 10,
    errors: 1,
    errorKinds: { timeout: 1 },
    redeliveries: 2,
    latencyMs: { p50, p95: max, p99: max, max },
  };
}

describe("ActionsView", () => {
  it("groups actions by group", () => {
    expect(groupOf("github.close_issue")).toBe("github");
    expect(groupOf("ask_user")).toBe("harness & built-in");
    const groups = groupActions([
      stats("github.a", 1, 2),
      stats("ask_user", 1, 2),
      stats("github.b", 1, 2),
    ]);
    expect(groups.map((g) => [g.name, g.calls, g.errors, g.redeliveries])).toEqual([
      ["github", 20, 2, 4],
      ["harness & built-in", 10, 1, 2],
    ]);
  });

  it("puts latencies on a log scale that ignores calls answered at once", () => {
    const scale = logScale([stats("a", 0, 0), stats("b", 140, 3_200)]);
    expect(scale.min).toBe(100);
    expect(scale.max).toBe(5_000);
    expect(scale.x(100)).toBe(0);
    expect(scale.x(5_000)).toBeCloseTo(1);
    expect(scale.x(0)).toBe(0);
    // Short: every step labelled; long: the major steps, every other one past five.
    expect(logTicks(scale).map((t) => t.ms)).toEqual([100, 200, 500, 1_000, 2_000, 5_000]);
    const long = logScale([stats("a", 15, 20), stats("b", 140, 3 * 3_600_000)]);
    expect(
      logTicks(long)
        .filter((t) => t.label)
        .map((t) => t.ms),
    ).toEqual([10, 1_000, 60_000, 3_600_000]);
  });

  it("shows latency, error rate and redeliveries per action over the window", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/actions?window=7d");
    const table = await screen.findByRole("table");
    const row = within(table)
      .getByRole("link", { name: "weather.forecast" })
      .closest("tr") as HTMLElement;
    expect(within(row).getAllByText(/%$/).length).toBeGreaterThan(0);
    const since = requested("/v1/actions/stats").at(-1)?.searchParams.get("since");
    expect(Date.now() - Date.parse(since ?? "")).toBeGreaterThan(7 * 86_400_000 - 60_000);
    expect(
      within(row)
        .getAllByRole("link")
        .some(
          (a) => a.getAttribute("href") === "/runs?q=action%3Aweather.forecast+error%3Atimeout",
        ),
    ).toBe(true);

    await user.click(screen.getByRole("button", { name: "24h" }));
    await waitFor(() => expect(search(router).get("window")).toBeNull());
  });

  it("opens from a link of the Tools view", async () => {
    const { router } = renderApp("/tools?window=7d");
    await screen.findByRole("table");
    expect(router.state.location.pathname).toBe("/actions");
    expect(search(router).get("window")).toBe("7d");
  });
});
