import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { problem } from "../../mocks/problem";
import type { Permission } from "../../model/types";
import { confirmInDialog, renderApp, search, signedIn } from "../../test/app";
import { server } from "../../test/server";

/** An answer the test gives when it chooses: `release` lets the request go on to the mocked API. */
function held() {
  let release: (answer?: Response) => void = () => {};
  const answer = new Promise<Response | undefined>((resolve) => {
    release = resolve;
  });
  return { answer, release };
}

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
    // chen has a display name: the views show it, its id on hover.
    expect(names().slice(0, 5)).toEqual([
      "Organization",
      "Defaults",
      "bob",
      "Chen Logistics",
      "dana",
    ]);
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
    // The first page of how many there are.
    expect(screen.getByText(/^50 of 1,2\d\d namespaces$/)).toBeInTheDocument();
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
    expect(
      within(row("Chen Logistics")).getByRole("img", { name: /^Runs this month/ }),
    ).toHaveAccessibleName("Runs this month: 4,120, of 20,000, 20%");
  });

  it("finds a namespace by the start of its display name, whatever the case", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp("/namespaces");
    await listed();
    const bar = screen.getByRole("combobox", { name: "Query" });
    await user.type(bar, "acme");
    // Suggested with its display name beside its id.
    expect(
      await screen.findByRole("option", { name: /user_7Kd02.*Acme Corp/ }),
    ).toBeInTheDocument();
    await user.keyboard("{Escape}");
    await user.clear(bar);
    await user.type(bar, "acme{Enter}");
    await waitFor(() => expect(names().slice(2)).toEqual(["Acme Corp"]));
    expect(requested("/v1/namespaces").some((u) => u.searchParams.get("prefix") === "acme")).toBe(
      true,
    );
    expect(row("Acme Corp").querySelector("a")).toHaveAttribute("href", "/namespaces/user_7Kd02");
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
    await waitFor(() => expect(names()[2]).toBe("Chen Logistics"));
    expect(search(router).get("order")).toBe("runs");
    expect(screen.getByRole("columnheader", { name: "Runs this month ↓" })).toHaveAttribute(
      "aria-sort",
      "descending",
    );

    const bar = screen.getByRole("combobox", { name: "Query" });
    await user.type(bar, "da");
    // The names that start with it are suggested as it is typed.
    expect(await screen.findByRole("option", { name: /dana/ })).toBeInTheDocument();
    await user.keyboard("{Enter}");
    await waitFor(() => expect(names().slice(2)).toEqual(["dana"]));
    expect(requested("/v1/namespaces").some((u) => u.searchParams.get("prefix") === "da")).toBe(
      true,
    );
    expect(search(router).get("q")).toBe("da");

    await user.click(screen.getByRole("button", { name: "Clear the query" }));
    await waitFor(() => expect(search(router).get("q")).toBeNull());
    await user.click(screen.getByRole("button", { name: /At 80% or more/ }));
    // By runs this month: bob, then user_7Kd02 (Acme Corp) at 83% of hers, then dana at 84% of her
    // CPU.
    await waitFor(() => expect(names().slice(2)).toEqual(["bob", "Acme Corp", "dana"]));
    expect(requested("/v1/namespaces").some((u) => u.searchParams.get("above") === "0.8")).toBe(
      true,
    );
    // What the page is, with the month's count, opens from the (i) beside its name.
    await user.click(screen.getByRole("button", { name: "About Namespaces" }));
    expect(await screen.findByText(/3 at 80% or more of a limit/)).toBeInTheDocument();
  });

  it("folds the namespaces under the organization's row", async () => {
    const user = userEvent.setup();
    renderApp("/namespaces?prefix=da");
    await waitFor(() => expect(names().slice(2)).toEqual(["dana"]));
    await user.click(screen.getByRole("button", { name: /Hide the namespaces of/ }));
    expect(names()).toHaveLength(1);
    expect(within(rows()[0] as HTMLElement).getByText(/· 1 namespace$/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /Show the namespaces of/ }));
    expect(names().slice(2)).toEqual(["dana"]);
  });

  it("loads more as many namespaces as shown and a page more, the rows shown kept", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp("/namespaces");
    await listed();
    const first = row("bob");
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() =>
      expect(screen.getByText(/^100 of 1,2\d\d namespaces$/)).toBeInTheDocument(),
    );
    expect(requested("/v1/namespaces").some((u) => u.searchParams.get("limit") === "100")).toBe(
      true,
    );
    // The same row, not one drawn again.
    expect(row("bob")).toBe(first);
  });

  it("keeps the namespaces shown, dimmed, until another order answers", async () => {
    const user = userEvent.setup();
    renderApp("/namespaces");
    await listed();
    const gate = held();
    server.use(
      http.get("*/v1/namespaces", async ({ request }) => {
        if (new URL(request.url).searchParams.get("order") !== "runs") return undefined;
        return gate.answer;
      }),
    );
    await user.click(screen.getByRole("button", { name: "Runs" }));
    const wrap = screen.getByRole("table").closest(".table-wrap");
    await waitFor(() => expect(wrap).toHaveClass("is-stale"));
    expect(names()[2]).toBe("bob");
    expect(screen.queryByText("Loading the namespaces…")).toBeNull();
    gate.release();
    await waitFor(() => expect(names()[2]).toBe("Chen Logistics"));
    expect(screen.getByRole("table").closest(".table-wrap")).not.toHaveClass("is-stale");
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
    // The month going on has no end: its query does not change as time passes.
    const runs = screen.getByRole("link", { name: "Open in Observability →" });
    expect(new URL(runs.getAttribute("href") ?? "", "http://x").searchParams.get("q")).toMatch(
      /^namespace:bob started:>=\S+$/,
    );
    expect(screen.getByRole("link", { name: "Open in Scripts →" })).toHaveAttribute(
      "href",
      "/scripts?q=namespace%3Abob",
    );
  });

  it("shows how its runs went this month, then its runs and its scripts", async () => {
    const { requested, callsTo } = renderApp("/namespaces/bob");
    await screen.findByRole("img", { name: /^Cumulative CPU time of bob/ });
    const figures = await screen.findByRole("region", { name: /^Runs in / });
    expect(within(figures).getByText("Runs started")).toBeInTheDocument();
    expect(within(figures).getByText("Completed")).toBeInTheDocument();
    expect(within(figures).getByText("Duration p95")).toBeInTheDocument();
    // The statistics are of its runs, over the month, by day.
    await waitFor(() =>
      expect(
        requested("/v1/runs/stats").some(
          (u) =>
            u.searchParams.get("q") === "namespace:bob" && u.searchParams.get("bucket") === "1d",
        ),
      ).toBe(true),
    );
    expect(screen.getByRole("region", { name: "What fails most" })).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Tools its runs call" })).toBeInTheDocument();
    const runs = screen.getByRole("region", { name: "Its runs" });
    await waitFor(() => expect(within(runs).getAllByRole("row").length).toBeGreaterThan(1));
    const scripts = screen.getByRole("region", { name: "Its scripts" });
    await waitFor(() => expect(within(scripts).getAllByRole("row").length).toBeGreaterThan(1));
    // Its scripts are counted over its runs (the titles of every script are read apart).
    expect(callsTo("/v1/scripts").some((c) => c.headers.get("Rewind-Namespace") === "bob")).toBe(
      true,
    );
  });

  it("filters and orders its runs from the table's headers, in the page's URL", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/namespaces/bob");
    const section = await screen.findByRole("region", { name: "Its runs" });
    await waitFor(() => expect(within(section).getAllByRole("row").length).toBeGreaterThan(1));
    // The scope is shown, and cannot be removed.
    const chips = within(section).getByRole("group", { name: "Filters" });
    expect(chips).toHaveTextContent("Namespace bob");
    expect(within(chips).queryByRole("button", { name: /^Remove/ })).toBeNull();

    await user.click(within(section).getByRole("button", { name: "Status" }));
    const menu = await screen.findByRole("dialog", { name: "Status: order and filters" });
    await user.click(within(menu).getByRole("button", { name: /^Status failed/ }));
    await waitFor(() => expect(search(router).get("q")).toBe("status:failed"));
    await waitFor(() =>
      expect(
        requested("/v1/runs").some((u) =>
          /^namespace:bob started:>=\S+ status:failed$/.test(u.searchParams.get("q") ?? ""),
        ),
      ).toBe(true),
    );
    await user.keyboard("{Escape}");
    expect(
      within(section).getByRole("button", { name: "Remove the filter status:failed" }),
    ).toBeVisible();

    await user.click(within(section).getByRole("button", { name: "Duration" }));
    const duration = await screen.findByRole("dialog", { name: "Duration: order and filters" });
    await user.click(within(duration).getByRole("button", { name: "Longest first" }));
    await waitFor(() => expect(search(router).get("sort")).toBe("duration"));
    await waitFor(() =>
      expect(requested("/v1/runs").some((u) => u.searchParams.get("sort") === "duration")).toBe(
        true,
      ),
    );
    expect(within(section).getByRole("columnheader", { name: "Duration" })).toHaveAttribute(
      "aria-sort",
      "descending",
    );

    // Removing the filter keeps the order.
    await user.click(
      within(section).getByRole("button", { name: "Remove the filter status:failed" }),
    );
    await waitFor(() => expect(search(router).get("q")).toBeNull());
    expect(search(router).get("sort")).toBe("duration");
  });

  it("shows its display name, its id on hover, and renames it", async () => {
    const user = userEvent.setup();
    const { callsTo } = renderApp("/namespaces/chen");
    await screen.findByRole("img", { name: /^Cumulative CPU time of Chen Logistics/ });
    const title = screen.getByRole("heading", { level: 1 });
    expect(title).toHaveTextContent("Chen Logistics");
    // The id stays one hover away, and in the card of the (i) beside the title.
    await user.hover(within(title).getByText("Chen Logistics"));
    const tip = await screen.findByText("Namespace", { selector: ".ns-tip-label" });
    expect(tip.parentElement).toHaveTextContent("Namespace chen");
    await user.unhover(within(title).getByText("Chen Logistics"));
    await user.click(screen.getByRole("button", { name: "About this page" }));
    expect(await screen.findByText(/^Id/)).toHaveTextContent("Id chen");

    await user.click(screen.getByRole("button", { name: "Rename" }));
    const form = screen.getByRole("form", { name: "Rename Chen Logistics" });
    const field = within(form).getByLabelText("Display name");
    await user.clear(field);
    await user.type(field, "  Chen & Sons  ");
    await user.click(within(form).getByRole("button", { name: "Save the name" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const [patch] = callsTo("/v1/namespaces/chen", "PATCH");
    expect(JSON.parse((await patch?.body) ?? "")).toEqual({ displayName: "Chen & Sons" });
    await waitFor(() =>
      expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("Chen & Sons"),
    );

    // Emptied, the name is removed and the views show the id again.
    await user.click(screen.getByRole("button", { name: "Rename" }));
    const again = screen.getByRole("form", { name: "Rename Chen & Sons" });
    await user.clear(within(again).getByLabelText("Display name"));
    await user.click(within(again).getByRole("button", { name: "Remove the name" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const second = callsTo("/v1/namespaces/chen", "PATCH")[1];
    expect(JSON.parse((await second?.body) ?? "")).toEqual({ displayName: null });
    await waitFor(() =>
      expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("chen"),
    );
  });

  it("gives its actions in the header one size", async () => {
    renderApp("/namespaces/bob");
    await screen.findByRole("img", { name: /^Cumulative CPU time of bob/ });
    for (const name of ["Rename", "Edit its limits", "Revoke its tokens", "Erase…"]) {
      const button = screen.getByRole("button", { name });
      expect(button).toHaveClass("btn");
      expect(button).not.toHaveClass("small");
      expect(button.querySelector("svg")).not.toBeNull();
    }
  });

  it("lists its exceptions with the defaults they replace", async () => {
    renderApp("/namespaces/chen");
    await screen.findByRole("img", { name: /^Cumulative CPU time of Chen Logistics/ });
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
    // In a dialog over the page, which Cancel closes without a change.
    const first = screen.getByRole("form", { name: "Limits of bob" });
    expect(screen.getByRole("dialog")).toContainElement(first);
    await user.click(within(first).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
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
    await confirmInDialog(user);
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
    expect(await screen.findByText(/being erased since/i)).toBeInTheDocument();
    // An end user being erased takes no change.
    expect(screen.queryByRole("button", { name: "Edit its limits" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Revoke its tokens" })).toBeNull();
  });

  it("shows its actions only to whoever holds their permission", async () => {
    // A reader sees, and changes nothing.
    renderApp("/namespaces/bob", { world: graceWith(["runs:read", "namespaces:read"]) });
    await screen.findByRole("img", { name: /^Cumulative CPU time of bob/ });
    expect(screen.getByRole("link", { name: "Open in Observability →" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Rename" })).toBeNull();
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

  it("shows a new name at once, and undoes it with the server's message when refused", async () => {
    const user = userEvent.setup();
    renderApp("/namespaces/chen");
    await screen.findByRole("img", { name: /^Cumulative CPU time of Chen Logistics/ });
    const gate = held();
    server.use(http.patch("*/v1/namespaces/:name", () => gate.answer));
    await user.click(screen.getByRole("button", { name: "Rename" }));
    const form = screen.getByRole("form", { name: "Rename Chen Logistics" });
    const field = within(form).getByLabelText("Display name");
    await user.clear(field);
    await user.type(field, "Chen & Sons");
    await user.click(within(form).getByRole("button", { name: "Save the name" }));
    // The page shows it before the server answers; the dialog waits for the answer.
    const title = () => screen.getByRole("heading", { level: 1, hidden: true });
    await waitFor(() => expect(title()).toHaveTextContent("Chen & Sons"));
    expect(within(form).getByRole("button", { name: "Saving…" })).toBeDisabled();
    expect(screen.getByRole("form", { name: "Rename Chen Logistics" })).toBe(form);

    gate.release(problem(400, "invalid-request", "displayName: refused by the test."));
    expect(await within(form).findByRole("alert")).toHaveTextContent(
      /Could not rename it.*refused by the test/,
    );
    expect(title()).toHaveTextContent("Chen Logistics");
  });

  it("shows the erasure at once, and keeps its dialog until the server answers", async () => {
    const user = userEvent.setup();
    renderApp("/namespaces/bob");
    await screen.findByRole("img", { name: /^Cumulative CPU time of bob/ });
    const gate = held();
    server.use(http.delete("*/v1/namespaces/:name", () => gate.answer));
    await user.click(screen.getByRole("button", { name: "Erase…" }));
    const form = screen.getByRole("form", { name: "Erase bob" });
    await user.type(within(form).getByLabelText("Type bob to confirm"), "bob");
    await user.click(within(form).getByRole("button", { name: "Erase bob" }));
    expect(await screen.findByText(/being erased since/i)).toBeInTheDocument();
    expect(within(form).getByRole("button", { name: "Erasing…" })).toBeDisabled();

    gate.release(
      HttpResponse.json(
        { type: "urn:rewind:problem:forbidden", title: "Forbidden", status: 403 },
        { status: 403, headers: { "Content-Type": "application/problem+json" } },
      ),
    );
    expect(await within(form).findByRole("alert")).toHaveTextContent(/403 · forbidden/);
    expect(screen.queryByText(/being erased since/i)).toBeNull();
    expect(screen.getByTestId("tokens")).toHaveTextContent("3");
  });
});
