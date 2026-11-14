import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { NAMESPACE_HEADER } from "../api/client";
import { createWorld } from "../mocks/fixtures";
import { NOW, renderApp, search, signedIn } from "../test/app";

function rows(): HTMLElement[] {
  return within(screen.getByRole("table")).getAllByRole("row").slice(1);
}

function queryInput(): HTMLInputElement {
  return screen.getByRole("combobox", { name: "Query" }) as HTMLInputElement;
}

/** The namespace each call to a path named. */
function namespacesSent(calls: { headers: Headers }[]): (string | null)[] {
  return calls.map((c) => c.headers.get(NAMESPACE_HEADER));
}

/** Opens the filter and picks a namespace among the suggestions for what is typed. */
async function pick(user: ReturnType<typeof userEvent.setup>, typed: string, name: RegExp) {
  await user.click(screen.getByLabelText(/^Namespace: /));
  const input = screen.getByRole("combobox", { name: "Namespace" });
  await user.type(input, typed);
  const list = screen.getByRole("listbox", { name: "Namespaces" });
  await user.click(await within(list).findByRole("option", { name }));
}

describe("the namespace filter", () => {
  it("narrows the views to the namespace picked: in the URL, and named with every request", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const { router, callsTo, requested } = renderApp("/runs", { world });
    await screen.findByRole("table");
    expect(screen.getByLabelText("Namespace: all namespaces")).toBeInTheDocument();
    // Over every namespace, no request names one.
    expect(namespacesSent(callsTo("/v1/runs")).every((ns) => ns === null)).toBe(true);

    // The suggestions are those whose name starts with what is typed, asked for as it is typed.
    await user.click(screen.getByLabelText("Namespace: all namespaces"));
    await user.type(screen.getByRole("combobox", { name: "Namespace" }), "bo");
    await waitFor(() =>
      expect(requested("/v1/namespaces").some((u) => u.searchParams.get("prefix") === "bo")).toBe(
        true,
      ),
    );
    const list = screen.getByRole("listbox", { name: "Namespaces" });
    await user.click(await within(list).findByRole("option", { name: /^bob/ }));
    await waitFor(() => expect(search(router).get("ns")).toBe("bob"));
    expect(screen.getByLabelText("Namespace: bob")).toBeInTheDocument();
    const bobs = world.runs.filter((r) => r.namespace === "bob").length;
    await waitFor(() => expect(rows()).toHaveLength(Math.min(50, bobs)));
    expect(namespacesSent(callsTo("/v1/runs"))).toContain("bob");
    expect(namespacesSent(callsTo("/v1/runs/stats"))).toContain("bob");
    // Narrowed to one end user, the runs need no namespace column.
    expect(screen.queryByRole("columnheader", { name: "Namespace" })).toBeNull();

    // It follows the links of the app, and narrows every view.
    await user.click(screen.getByRole("link", { name: "Overview" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/overview"));
    expect(search(router).get("ns")).toBe("bob");
    await waitFor(() =>
      expect(namespacesSent(callsTo("/v1/runs/failures")).some((ns) => ns === "bob")).toBe(true),
    );
    await user.click(screen.getByRole("link", { name: "Scripts" }));
    await waitFor(() =>
      expect(namespacesSent(callsTo("/v1/scripts")).some((ns) => ns === "bob")).toBe(true),
    );
    expect(search(router).get("ns")).toBe("bob");

    // Back to every namespace.
    const before = callsTo("/v1/scripts").length;
    await user.click(screen.getByRole("button", { name: "Show every namespace" }));
    await waitFor(() => expect(search(router).get("ns")).toBeNull());
    await waitFor(() => expect(callsTo("/v1/scripts").length).toBeGreaterThan(before));
    expect(namespacesSent(callsTo("/v1/scripts").slice(before))).toEqual(
      expect.arrayContaining([null]),
    );
    expect(screen.getByLabelText("Namespace: all namespaces")).toBeInTheDocument();
  });

  it("keeps the namespace through a run's page", async () => {
    const user = userEvent.setup();
    const { router, callsTo } = renderApp("/runs?q=status%3Acompleted&ns=alice");
    await screen.findByRole("table");
    const link = within(rows()[0] as HTMLElement).getAllByRole("link")[0] as HTMLElement;
    expect(link.getAttribute("href")).toMatch(/\?ns=alice$/);
    await user.click(link);
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/runs\/.+/));
    expect(search(router).get("ns")).toBe("alice");
    const runCalls = callsTo(router.state.location.pathname.replace("/runs/", "/v1/runs/"));
    expect(namespacesSent(runCalls)).toContain("alice");
    // The run's header names its namespace.
    expect(await screen.findByRole("link", { name: "alice" })).toHaveAttribute(
      "href",
      "/runs?ns=alice",
    );
  });

  it("finds no run of another namespace", async () => {
    const world = createWorld({ now: NOW });
    const bob = world.runs.find((r) => r.namespace === "bob");
    renderApp(`/runs/${bob?.id}?ns=alice`, { world });
    expect(await screen.findByRole("alert")).toHaveTextContent(/not-found/);
  });

  it("leads from a run's page back to the runs when another namespace is picked", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW });
    const run = world.runs.find((r) => r.namespace === "alice");
    const { router } = renderApp(`/runs/${run?.id}`, { world });
    await screen.findByRole("link", { name: "alice" });
    await pick(user, "da", /^dana/);
    await waitFor(() => expect(router.state.location.pathname).toBe("/runs"));
    expect(search(router).get("ns")).toBe("dana");
  });

  it("takes a namespace typed, for a role that cannot list them", async () => {
    const user = userEvent.setup();
    const world = signedIn("grace@example.net");
    world.access.roles.get("org_01ACME")?.push({
      name: "runs-only",
      builtIn: false,
      description: "Reads the runs.",
      permissions: ["runs:read"],
    });
    world.access.grants
      .get("org_01ACME")
      ?.set("user:user_01GRACE", { role: "runs-only", allowedLabels: {} });
    const { router, requested } = renderApp("/runs", { world });
    await screen.findByRole("table");
    await user.click(screen.getByLabelText("Namespace: all namespaces"));
    const input = screen.getByRole("combobox", { name: "Namespace" });
    expect(input).toHaveAttribute("placeholder", "Type a namespace");
    await user.type(input, "bad name");
    expect(screen.getByText(/made of letters, digits/)).toBeInTheDocument();
    await user.clear(input);
    await user.type(input, "erin{Enter}");
    await waitFor(() => expect(search(router).get("ns")).toBe("erin"));
    expect(requested("/v1/namespaces")).toHaveLength(0);
  });
});

describe("the runs of every namespace", () => {
  it("show each run's namespace, in a column and a facet", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/runs");
    await screen.findByRole("table");
    expect(screen.getByRole("columnheader", { name: "Namespace" })).toBeInTheDocument();
    const facet = await screen.findByRole("region", { name: "Namespace" });
    expect(within(facet).getByText("Among the runs shown")).toBeInTheDocument();
    await user.click(within(facet).getByRole("button", { name: /^Namespace bob, \d+ runs$/ }));
    await waitFor(() => expect(search(router).get("q")).toBe("namespace:bob"));
    await waitFor(() =>
      expect(rows().every((r) => within(r).queryByRole("button", { name: "bob" }) !== null)).toBe(
        true,
      ),
    );
    // A run's namespace adds itself to the query.
    await user.click(screen.getByRole("button", { name: "Clear the query" }));
    await waitFor(() => expect(search(router).get("q")).toBeNull());
    await user.click(
      within(rows()[0] as HTMLElement).getAllByTitle(/^Only runs of /)[0] as HTMLElement,
    );
    await waitFor(() => expect(search(router).get("q")).toMatch(/^namespace:/));
  });

  it("suggest the namespaces after namespace:, as they are typed", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp("/runs");
    await screen.findByRole("table");
    await user.click(queryInput());
    await user.type(queryInput(), "names");
    const fields = await screen.findByRole("listbox", { name: "Suggestions" });
    expect(within(fields).getByRole("option", { name: /^namespace:/ })).toBeInTheDocument();
    await user.keyboard("{ArrowDown}{Enter}");
    expect(queryInput().value).toBe("namespace:");
    await user.type(queryInput(), "ch");
    const values = await screen.findByRole("listbox", { name: "Suggestions" });
    expect(
      await within(values).findByRole("option", { name: /namespace:chen.*runs this month/ }),
    ).toBeInTheDocument();
    expect(requested("/v1/namespaces").some((u) => u.searchParams.get("prefix") === "ch")).toBe(
      true,
    );
  });
});
