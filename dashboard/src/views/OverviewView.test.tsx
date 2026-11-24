import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { renderApp, search } from "../test/app";

function hrefOf(link: HTMLElement): URLSearchParams {
  return new URLSearchParams((link.getAttribute("href") ?? "").split("?")[1] ?? "");
}

describe("OverviewView", () => {
  it("is the home page", async () => {
    const { router } = renderApp("/");
    expect(await screen.findByRole("heading", { name: "Overview" })).toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/overview");
  });

  it("shows the figures of the window, each a link to the runs behind it", async () => {
    renderApp("/overview?q=%40env%3Aprod");
    const figures = await screen.findByRole("region", { name: "Figures" });
    const started = await within(figures).findByRole("link", { name: /Runs started/ });
    expect(hrefOf(started).get("q")).toBe("@env:prod started:>now-24h");
    expect(started).toHaveTextContent(/previous 24h/);
    const completed = within(figures).getByRole("link", { name: /Completed/ });
    expect(completed).toHaveTextContent(/%.*of the \d+ runs that ended/);
    expect(hrefOf(completed).get("q")).toBe("@env:prod started:>now-24h status:completed");
    const failed = within(figures).getByRole("link", { name: /Failed/ });
    expect(failed.getAttribute("href")).toMatch(/^\/runs\/failures\?q=/);
    expect(failed).toHaveTextContent(/aborted, \d+ cancelled/);
    // Waiting now: whenever they started, by what they wait for (a signal, a timer, a harness
    // action, an approval, a parked call, a quarantine); not the triage of the test environment.
    const waiting = within(figures).getByRole("link", { name: /Waiting now/ });
    await waitFor(() => expect(waiting).toHaveTextContent(/Waiting now\s*6\s*1 on a signal/));
    expect(waiting).toHaveAttribute("href", "/runs/waiting?q=%40env%3Aprod");
    expect(within(figures).getByRole("link", { name: /Duration p95/ })).toHaveTextContent(/p50/);
    const quarantined = within(figures).getByRole("link", { name: /Quarantined/ });
    expect(quarantined).toHaveTextContent(/Quarantined\s*1/);
    expect(hrefOf(quarantined).get("q")).toBe("@env:prod waiting:quarantined");
  });

  it("stacks the runs started by outcome over time, each segment a link", async () => {
    const { requested } = renderApp("/overview");
    const chart = await screen.findByRole("region", { name: "Runs by outcome" });
    const bars = await within(chart).findByRole("group", {
      name: /Runs started, by status, 24 buckets/,
    });
    const segments = within(bars).getAllByRole("link");
    expect(segments.length).toBeGreaterThan(0);
    const completed = segments.find((s) => /completed/.test(s.getAttribute("aria-label") ?? ""));
    const q = hrefOf(completed as HTMLElement).get("q") ?? "";
    expect(q).toMatch(
      /^status:completed started:\d{4}-\d\d-\d\dT[\d:]+Z\.\.\d{4}-\d\d-\d\dT[\d:]+Z$/,
    );
    expect(within(chart).getByText("completed")).toBeInTheDocument();
    const stats = requested("/v1/runs/stats").find((u) => u.searchParams.get("interval"));
    expect(stats?.searchParams.get("interval")).toBe("1h");
  });

  it("lists what fails most and the failing actions, each a link", async () => {
    renderApp("/overview?window=7d");
    const fails = await screen.findByRole("region", { name: "What fails most" });
    const top = (await within(fails).findAllByRole("link")).filter((a) =>
      a.getAttribute("href")?.startsWith("/runs?"),
    );
    expect(top.length).toBeGreaterThan(0);
    expect(hrefOf(top[0] as HTMLElement).get("q")).toMatch(
      /^fingerprint:[0-9a-f]{16} ended:>now-7d$/,
    );
    expect(within(fails).getByRole("link", { name: "Every failure →" })).toHaveAttribute(
      "href",
      "/runs/failures?window=7d",
    );
    const actions = screen.getByRole("region", { name: "Failing actions" });
    const forecast = await within(actions).findByRole("link", { name: /weather\.forecast/ });
    expect(hrefOf(forecast).get("q")).toMatch(
      /^started:>now-7d action:weather\.forecast error:(timeout|unavailable)$/,
    );
  });

  it("changes the window, and asks for its intervals", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/overview");
    await screen.findByRole("region", { name: "Figures" });
    await user.click(screen.getByRole("button", { name: "7d" }));
    await waitFor(() => expect(search(router).get("window")).toBe("7d"));
    await waitFor(() =>
      expect(requested("/v1/runs/stats").some((u) => u.searchParams.get("interval") === "6h")).toBe(
        true,
      ),
    );
    const since = requested("/v1/runs/stats")
      .find((u) => u.searchParams.get("interval") === "6h")
      ?.searchParams.get("since");
    expect(Date.now() - Date.parse(since ?? "")).toBeGreaterThan(7 * 86_400_000 - 60_000);
    // The window follows to Runs.
    expect(
      within(screen.getByRole("navigation", { name: "Views" })).getByRole("link", {
        name: "Runs",
      }),
    ).toHaveAttribute("href", "/runs?window=7d");
  });
});
