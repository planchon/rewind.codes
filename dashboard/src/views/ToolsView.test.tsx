import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { ToolStats } from "../api/types";
import { renderApp, search } from "../test/app";
import { groupTools, logScale, namespaceOf } from "./ToolsView";

function stats(tool: string, p50: number, max: number): ToolStats {
  return {
    tool,
    calls: 10,
    errors: 1,
    errorKinds: { timeout: 1 },
    retries: 2,
    latencyMs: { p50, p95: max, p99: max, max },
  };
}

describe("ToolsView", () => {
  it("groups tools by namespace", () => {
    expect(namespaceOf("github.close_issue")).toBe("github");
    expect(namespaceOf("ask_user")).toBe("host & built-in");
    const groups = groupTools([
      stats("github.a", 1, 2),
      stats("ask_user", 1, 2),
      stats("github.b", 1, 2),
    ]);
    expect(groups.map((g) => [g.name, g.calls, g.errors, g.retries])).toEqual([
      ["github", 20, 2, 4],
      ["host & built-in", 10, 1, 2],
    ]);
  });

  it("puts latencies on a log scale that ignores calls answered at once", () => {
    const scale = logScale([stats("a", 0, 0), stats("b", 140, 3_200)]);
    expect(scale.min).toBe(100);
    expect(scale.x(100)).toBe(0);
    expect(scale.x(3_200)).toBeCloseTo(1);
    expect(scale.x(0)).toBe(0);
  });

  it("shows latency, error rate and retries per tool over the window", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/tools?window=7d");
    const table = await screen.findByRole("table");
    const row = within(table)
      .getByRole("link", { name: "weather.forecast" })
      .closest("tr") as HTMLElement;
    expect(within(row).getAllByText(/%$/).length).toBeGreaterThan(0);
    const since = requested("/v1/tools/stats").at(-1)?.searchParams.get("since");
    expect(Date.now() - Date.parse(since ?? "")).toBeGreaterThan(7 * 86_400_000 - 60_000);
    expect(
      within(row)
        .getAllByRole("link")
        .some(
          (a) =>
            a.getAttribute("href") === "/observability?q=tool%3Aweather.forecast+error%3Atimeout",
        ),
    ).toBe(true);

    await user.click(screen.getByRole("button", { name: "24h" }));
    await waitFor(() => expect(search(router).get("window")).toBeNull());
  });
});
