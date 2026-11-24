import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { CODES, WORKFLOWS } from "../../mocks/fixtures";
import { renderApp, search, signedIn } from "../../test/app";

/** The rows of the table of workflows, without its header. */
async function rows(): Promise<HTMLElement[]> {
  const table = await screen.findByRole("table");
  await waitFor(() => expect(within(table).getAllByRole("row").length).toBeGreaterThan(1));
  return within(table).getAllByRole("row").slice(1);
}

/** The row whose workflow shows as `label`. */
function row(all: HTMLElement[], label: string): HTMLElement {
  const found = all.find((r) => within(r).queryByRole("link", { name: label }));
  if (!found) throw new Error(`no row ${label}`);
  return found;
}

describe("WorkflowsView", () => {
  it("lists each workflow as it shows, with its scope, its version and its runs of the window", async () => {
    renderApp("/workflows?window=30d");
    expect(await screen.findByRole("heading", { name: "Workflows" })).toBeInTheDocument();
    const all = await rows();
    // By its name, with its description's first line under it.
    const triage = within(row(all, "triage"));
    expect(triage.getByText("Organization")).toBeInTheDocument();
    expect(triage.getByText("v3")).toBeInTheDocument();
    expect(
      triage.getByText("Triage the stale issues of a repository, after approval."),
    ).toBeVisible();
    // Without a name, by its code's first comment line; without either, by its short id.
    const forecast = within(row(all, "One-shot lookup started by the support agent."));
    expect(forecast.getByRole("link", { name: "64" })).toHaveAttribute(
      "href",
      `/runs?q=workflow%3A${WORKFLOWS.forecast}`,
    );
    expect(forecast.getByRole("button", { name: "weather.forecast" })).toBeInTheDocument();
    expect(forecast.getByTitle(/\d+ completed, \d+ failed/)).toBeInTheDocument();
    const chen = within(row(all, `wf-${WORKFLOWS.chenDigest.slice(3, 9)}`));
    expect(chen.getByText("Chen Logistics")).toBeInTheDocument();
    // A fork says what it forks.
    const fork = all.find((r) =>
      within(r)
        .queryByRole("link", { name: "triage" })
        ?.getAttribute("href")
        ?.endsWith(WORKFLOWS.triageFork),
    ) as HTMLElement;
    expect(within(fork).getByTitle("Fork of triage v2")).toHaveTextContent("fork");
    expect(within(fork).getByText("dana")).toBeInTheDocument();
    // A row opens its workflow.
    expect(within(row(all, "triage")).getByRole("link", { name: "triage" })).toHaveAttribute(
      "href",
      `/workflows/${WORKFLOWS.triage}`,
    );
  });

  it("filters by action in the browser, from the query bar", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/workflows?window=30d");
    await rows();
    await user.click(screen.getAllByRole("button", { name: "crm.provision" })[0] as HTMLElement);
    await waitFor(() => expect(search(router).get("q")).toBe("action:crm.provision"));
    expect(screen.getByRole("combobox", { name: "Query" })).toHaveValue("action:crm.provision");
    await waitFor(() =>
      expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(2),
    );
    // The list takes no action: the workflows' actions are read from its rows.
    expect(requested("/v1/workflows").some((u) => u.searchParams.has("action"))).toBe(false);
  });

  it("counts the window's runs, the last 24 hours by default", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/workflows");
    await rows();
    const since = (u: URL) => Date.parse(u.searchParams.get("since") ?? "");
    const listed = () =>
      requested("/v1/workflows").filter((u) => u.searchParams.get("limit") !== "500");
    const day = listed().at(-1) as URL;
    expect(Date.now() - since(day)).toBeGreaterThanOrEqual(86_400_000 - 60_000);
    expect(Date.now() - since(day)).toBeLessThan(86_400_000 + 60_000);

    await user.click(screen.getByRole("button", { name: "30d" }));
    await waitFor(() => expect(search(router).get("window")).toBe("30d"));
    await waitFor(() =>
      expect(Date.now() - since(listed().at(-1) as URL)).toBeGreaterThanOrEqual(
        30 * 86_400_000 - 60_000,
      ),
    );
  });

  it("underlines a label, which the counters know nothing of, and what the list cannot read", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/workflows?label=agent:triage");
    // An older link's filter is read as the query it means, which the list cannot read.
    await waitFor(() => expect(search(router).get("q")).toBe("@agent:triage"));
    expect(search(router).get("label")).toBeNull();
    expect(await screen.findByRole("alert")).toHaveTextContent(/know no labels/);
    expect(requested("/v1/workflows")).toHaveLength(0);

    const bar = screen.getByRole("combobox", { name: "Query" });
    await user.clear(bar);
    await user.type(bar, "status:failed{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent(/searched by namespace/);
    expect(bar).toHaveAttribute("aria-invalid", "true");
    expect(screen.queryByRole("table")).toBeNull();
  });

  it("narrows to one namespace with Rewind-Namespace, and its links to the runs too", async () => {
    const { callsTo } = renderApp("/workflows?q=namespace:chen&window=30d");
    const all = await rows();
    const listed = callsTo("/v1/workflows").filter((c) => c.url.searchParams.has("since"));
    await waitFor(() => expect(listed.at(-1)?.headers.get("Rewind-Namespace")).toBe("chen"));
    expect(listed.at(-1)?.url.searchParams.get("q")).toBe("namespace:chen");
    // chen's workflows and the organization's, no other namespace's.
    const scopes = all.map((r) => within(r).getAllByRole("cell")[1]?.textContent);
    expect(new Set(scopes)).toEqual(new Set(["Chen Logistics", "Organization"]));
    const runs = within(row(all, `wf-${WORKFLOWS.chenDigest.slice(3, 9)}`)).getByTitle("Its runs");
    expect(new URL(runs.getAttribute("href") ?? "", "http://x").searchParams.get("q")).toBe(
      `workflow:${WORKFLOWS.chenDigest} namespace:chen`,
    );
  });

  it("shows no counts to a grant limited to labels, whose workflows carry them", async () => {
    renderApp("/workflows", { world: signedIn("alan@example.net", "org_01ACME") });
    const all = await rows();
    expect(screen.queryByRole("columnheader", { name: "Runs" })).toBeNull();
    // Those whose versions carry agent:triage: triage, dana's fork, alice's two.
    expect(all).toHaveLength(4);
    expect(within(row(all, "triage_stale_issues")).getByText("v2")).toBeInTheDocument();
    expect(within(all[0] as HTMLElement).getAllByRole("cell")).toHaveLength(3);
  });

  it("opens from the links of Scripts and Testing", async () => {
    const scripts = renderApp("/scripts?window=7d");
    await rows();
    expect(scripts.router.state.location.pathname).toBe("/workflows");
    expect(search(scripts.router).get("window")).toBe("7d");
    scripts.unmount();

    const testing = renderApp("/testing");
    await rows();
    expect(testing.router.state.location.pathname).toBe("/workflows");
    testing.unmount();

    // A script's page: the workflow whose version has that code.
    const script = renderApp(`/scripts/${CODES.FORECAST.digest}`);
    await waitFor(() =>
      expect(script.router.state.location.pathname).toBe(`/workflows/${WORKFLOWS.forecast}`),
    );
  });
});
