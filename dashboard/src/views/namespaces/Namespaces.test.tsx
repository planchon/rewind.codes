import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Permission } from "../../api/types";
import { renderApp, search, signedIn } from "../../test/app";

/** Gives Grace, in Acme, a role made of these permissions. */
function graceWith(permissions: Permission[]) {
  const world = signedIn("grace@example.net");
  world.access.roles.get("org_01ACME")?.push({
    name: "custom",
    builtIn: false,
    description: "A role for the tests.",
    permissions,
  });
  world.access.grants
    .get("org_01ACME")
    ?.set("user:user_01GRACE", { role: "custom", allowedLabels: {} });
  return world;
}

/** The rows of the table, header row excluded. */
function rows(): HTMLElement[] {
  return within(screen.getByRole("table")).getAllByRole("row").slice(1);
}

/** The name in each row's header cell. */
function names(): string[] {
  return rows().map((r) => within(r).getAllByRole("rowheader")[0]?.firstChild?.textContent ?? "");
}

/** Waits for the table to list the namespaces. */
async function listed() {
  await waitFor(() => expect(names()).toContain("bob"));
}

function row(name: string): HTMLElement {
  const found = rows().find((r) => within(r).queryByRole("link", { name }) !== null);
  if (!found) throw new Error(`no row ${name}`);
  return found;
}

describe("the Namespaces view", () => {
  it("shows the ceiling, the defaults, then the namespaces using the most CPU", async () => {
    const { requested } = renderApp("/namespaces");
    await listed();
    expect(names().slice(0, 5)).toEqual(["Organization", "Defaults", "bob", "chen", "dana"]);
    expect(requested("/v1/namespaces").some((u) => u.searchParams.get("order") === "cpu")).toBe(
      true,
    );
    // The ceiling's row: the organization's totals against the operator's ceiling.
    const ceiling = rows()[0] as HTMLElement;
    expect(within(ceiling).getByText("operator's ceiling, on the totals")).toBeInTheDocument();
    expect(within(ceiling).getByRole("img", { name: /^Active runs: .* of 5,000/ })).toBeVisible();
    // The defaults, with the limits that are not meters.
    const defaults = rows()[1] as HTMLElement;
    expect(within(defaults).getByText("TTL 7d · script 300 lines")).toBeInTheDocument();
    expect(within(defaults).getByText("3,000")).toBeInTheDocument();
  });

  it("colors each quota from 80 % and keeps a CPU above its quota full, with its overrun", async () => {
    renderApp("/namespaces");
    await listed();
    const bob = row("bob");
    const cpu = within(bob).getByRole("img", { name: /^CPU this month/ });
    expect(cpu).toHaveAccessibleName("CPU this month: 3h 12m, of 2h, 160%");
    expect(cpu).toHaveClass("level-critical");
    expect(within(cpu).getByText("160%")).toBeInTheDocument();
    const bar = cpu.querySelector(".meter-bar");
    expect(bar?.getAttribute("width")).toBe("100");
    expect(within(bob).getByRole("img", { name: /^Runs this month: 2,950, of 3,000/ })).toHaveClass(
      "level-warning",
    );
    expect(within(bob).getByRole("img", { name: /^Active runs: 10, of 10/ })).toHaveClass(
      "level-critical",
    );
    // Its worst alert of the month, under its name.
    expect(within(bob).getByText(/100%$/)).toHaveClass("critical");
    expect(within(row("dana")).getByRole("img", { name: /^CPU this month/ })).toHaveClass(
      "level-warning",
    );
    // chen pays: its exceptions lift the defaults.
    expect(within(row("chen")).getByRole("img", { name: /^Runs this month/ })).toHaveAccessibleName(
      "Runs this month: 4,120, of 20,000, 20%",
    );
  });

  it("shows an end user the customer blocked", async () => {
    renderApp("/namespaces?prefix=erin");
    await waitFor(() => expect(names().slice(2)).toEqual(["erin"]));
    const active = within(row("erin")).getByRole("img", { name: /^Active runs/ });
    expect(active).toHaveAccessibleName("Active runs: 0, of 0, blocked");
    expect(within(active).getByText("blocked")).toBeInTheDocument();
    expect(within(row("erin")).getByText("exceptions: active runs")).toBeInTheDocument();
  });

  it("sorts by runs, searches by the start of a name, and keeps those at 80 % or more", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/namespaces");
    await listed();

    await user.click(screen.getByRole("button", { name: "Runs" }));
    await waitFor(() => expect(names()[2]).toBe("chen"));
    expect(search(router).get("order")).toBe("runs");
    expect(screen.getByRole("columnheader", { name: "Runs this month ↓" })).toHaveAttribute(
      "aria-sort",
      "descending",
    );

    await user.type(screen.getByRole("searchbox", { name: "Search a namespace" }), "da");
    await waitFor(() => expect(names().slice(2)).toEqual(["dana"]));
    expect(requested("/v1/namespaces").some((u) => u.searchParams.get("prefix") === "da")).toBe(
      true,
    );
    expect(search(router).get("prefix")).toBe("da");

    await user.clear(screen.getByRole("searchbox", { name: "Search a namespace" }));
    await user.click(screen.getByRole("button", { name: /At 80% or more/ }));
    // By runs this month: bob, then user_7Kd02 at 83% of hers, then dana at 84% of her CPU.
    await waitFor(() => expect(names().slice(2)).toEqual(["bob", "user_7Kd02", "dana"]));
    expect(requested("/v1/namespaces").some((u) => u.searchParams.get("above") === "0.8")).toBe(
      true,
    );
    expect(screen.getByText(/3 at 80% or more of a limit/)).toBeInTheDocument();
  });

  it("lists the alerts of the month, each sent to the webhooks", async () => {
    renderApp("/namespaces");
    const alerts = await screen.findByRole("region", { name: "Alerts this month" });
    const items = within(alerts).getAllByRole("listitem");
    expect(items.some((li) => /CPU 100%.*bob/.test(li.textContent ?? ""))).toBe(true);
    expect(items.some((li) => /runs this month 80%.*user_7Kd02/.test(li.textContent ?? ""))).toBe(
      true,
    );
  });

  it("changes the defaults, for whoever holds namespaces:write", async () => {
    const user = userEvent.setup();
    const { callsTo } = renderApp("/namespaces");
    await listed();
    await user.click(screen.getByRole("button", { name: "Edit the defaults" }));
    const form = screen.getByRole("form", { name: "Default limits" });
    const cpu = within(form).getByLabelText("CPU per month");
    expect(cpu).toHaveValue("2h");
    await user.clear(cpu);
    await user.type(cpu, "3h");
    await user.clear(within(form).getByLabelText("Longest TTL"));
    await user.click(within(form).getByRole("button", { name: "Save the defaults" }));
    await waitFor(() => expect(screen.queryByRole("form", { name: "Default limits" })).toBeNull());
    const [put] = callsTo("/v1/organization/namespace-defaults", "PUT");
    expect(JSON.parse((await put?.body) ?? "")).toEqual({
      defaults: {
        activeRuns: 10,
        runsPerMonth: 3000,
        cpuPerMonth: "3h",
        scripts: 500,
        maxScriptLines: 300,
      },
    });
    // With a quota of 3h, bob is still above his.
    await waitFor(() =>
      expect(within(row("bob")).getByRole("img", { name: /^CPU this month/ })).toHaveAccessibleName(
        "CPU this month: 3h 12m, of 3h, 106%",
      ),
    );
  });

  it("tells a member without namespaces:read why, and keeps the view out of the navigation", async () => {
    const world = graceWith(["runs:read"]);
    renderApp("/namespaces", { world });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      /does not give the permission namespaces:read/,
    );
    expect(screen.queryByRole("link", { name: "Namespaces" })).toBeNull();
  });
});

describe("a namespace's page", () => {
  it("shows its CPU against its quota over the month, its alerts, tokens and exceptions", async () => {
    renderApp("/namespaces/bob");
    const chart = await screen.findByRole("img", { name: /^Cumulative CPU time of bob/ });
    expect(chart.getAttribute("aria-label")).toMatch(
      /3h 12m by .*, against a quota of 2h, 80% reached on .*, 100% on /,
    );
    const alerts = screen.getByRole("region", { name: "Alerts" });
    const chips = within(alerts)
      .getAllByRole("listitem")
      .map((li) => li.firstChild?.textContent);
    expect(chips).toEqual(
      expect.arrayContaining(["CPU 100%", "CPU 80%", "scripts 80%", "active runs 100%"]),
    );
    expect(within(alerts).getAllByText(/webhook sent/).length).toBeGreaterThan(0);
    expect(screen.getByTestId("tokens")).toHaveTextContent("3");
    expect(screen.getByText("none: the defaults apply")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "See its runs" })).toHaveAttribute(
      "href",
      "/runs?ns=bob",
    );
  });

  it("lists its exceptions with the defaults they replace", async () => {
    renderApp("/namespaces/chen");
    await screen.findByRole("img", { name: /^Cumulative CPU time of chen/ });
    expect(screen.getByText(/Runs per month:/).closest("li")).toHaveTextContent(
      "Runs per month: 20,000 (default 3,000)",
    );
    expect(screen.getByText(/CPU per month:/).closest("li")).toHaveTextContent(
      "CPU per month: 10h (default 2h)",
    );
  });

  it("reads another month", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/namespaces/alice");
    await screen.findByRole("img", { name: /^Cumulative CPU time of alice/ });
    expect(screen.getByRole("button", { name: "The month after" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "The month before" }));
    await waitFor(() => expect(search(router).get("month")).not.toBeNull());
    const month = search(router).get("month");
    await waitFor(() =>
      expect(
        requested("/v1/namespaces/alice").some((u) => u.searchParams.get("month") === month),
      ).toBe(true),
    );
  });

  it("changes its limits, revokes its tokens and erases it", async () => {
    const user = userEvent.setup();
    const { callsTo } = renderApp("/namespaces/bob");
    await screen.findByRole("img", { name: /^Cumulative CPU time of bob/ });

    await user.click(screen.getByRole("button", { name: "Edit its limits" }));
    const form = screen.getByRole("form", { name: "Limits of bob" });
    expect(within(form).getByLabelText("CPU per month")).toHaveAttribute(
      "placeholder",
      "default: 2h",
    );
    await user.type(within(form).getByLabelText("CPU per month"), "5h");
    await user.type(within(form).getByLabelText("Active runs"), "0");
    await user.click(within(form).getByRole("button", { name: "Save the limits" }));
    await waitFor(() => expect(screen.queryByRole("form", { name: "Limits of bob" })).toBeNull());
    const [put] = callsTo("/v1/namespaces/bob", "PUT");
    expect(JSON.parse((await put?.body) ?? "")).toEqual({
      exceptions: { activeRuns: 0, cpuPerMonth: "5h" },
    });
    expect(await screen.findByText("CPU per month:", { exact: false })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Revoke its tokens" }));
    await user.click(screen.getByRole("button", { name: "Yes" }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      /3 tokens revoked; the signed tokens issued before .* are refused/,
    );
    expect(callsTo("/v1/namespaces/bob/tokens", "DELETE")).toHaveLength(1);
    await waitFor(() => expect(screen.getByTestId("tokens")).toHaveTextContent("0"));

    await user.click(screen.getByRole("button", { name: "Erase…" }));
    const erase = screen.getByRole("form", { name: "Erase bob" });
    const confirm = within(erase).getByRole("button", { name: "Erase bob" });
    expect(confirm).toBeDisabled();
    await user.type(within(erase).getByLabelText("Type bob to confirm"), "bob");
    await user.click(confirm);
    expect(await screen.findByText(/The erasure of/)).toBeInTheDocument();
    expect(callsTo("/v1/namespaces/bob", "DELETE")).toHaveLength(1);
    expect(await screen.findByText(/being erased since/)).toBeInTheDocument();
    // An end user being erased takes no change.
    expect(screen.queryByRole("button", { name: "Edit its limits" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Revoke its tokens" })).toBeNull();
  });

  it("shows its actions only to whoever holds their permission", async () => {
    // A reader sees, and changes nothing.
    renderApp("/namespaces/bob", { world: graceWith(["runs:read", "namespaces:read"]) });
    await screen.findByRole("img", { name: /^Cumulative CPU time of bob/ });
    expect(screen.getByRole("link", { name: "See its runs" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Edit its limits" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Revoke its tokens" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Erase…" })).toBeNull();
  });

  it("offers to revoke the tokens to whoever holds tokens:write alone", async () => {
    renderApp("/namespaces/bob", {
      world: graceWith(["runs:read", "namespaces:read", "tokens:write"]),
    });
    await screen.findByRole("img", { name: /^Cumulative CPU time of bob/ });
    expect(screen.getByRole("button", { name: "Revoke its tokens" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Edit its limits" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Erase…" })).toBeNull();
  });

  it("says when there is no such namespace", async () => {
    renderApp("/namespaces/nobody");
    expect(await screen.findByRole("alert")).toHaveTextContent(/404 · not-found/);
  });
});
