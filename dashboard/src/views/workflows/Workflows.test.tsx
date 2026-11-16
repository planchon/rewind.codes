import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { WORKFLOWS } from "../../mocks/fixtures";
import { renderApp, search, signedIn } from "../../test/app";

/** The panel of a version, by its `name@<12 digits>`. */
async function versionPanel(ref: string): Promise<HTMLElement> {
  return screen.findByRole("region", { name: `Version ${ref}` });
}

/** The panel of the latest version, once the versions are read. */
async function latestPanel(): Promise<HTMLElement> {
  await screen.findAllByRole("region", { name: /^Version / });
  return screen.getAllByRole("region", { name: /^Version / })[0] as HTMLElement;
}

/** The suite's rows of a version's panel: the tag, then the test's name. */
function suiteRows(panel: HTMLElement): string[] {
  return within(panel)
    .getAllByRole("listitem")
    .filter((li) => li.classList.contains("suite-test"))
    .map((li) => li.textContent ?? "");
}

describe("WorkflowsView", () => {
  it("lists each workflow with its latest version and the size of its suite", async () => {
    renderApp("/workflows");
    const table = await screen.findByRole("table");
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows.map((r) => within(r).getAllByRole("link")[0]?.textContent)).toEqual([
      "digest",
      "triage",
    ]);
    const triage = within(rows[1] as HTMLElement);
    expect(triage.getByRole("link", { name: "triage" })).toHaveAttribute(
      "href",
      "/workflows/triage",
    );
    expect(
      triage.getByText("Triage the stale issues of a repository, after approval."),
    ).toBeVisible();
    // The third version: two inherited tests and two recorded ones.
    expect(triage.getByText("4")).toBeInTheDocument();
  });

  it("is in the sidebar, next to Scripts", async () => {
    renderApp("/workflows");
    const nav = await screen.findByRole("navigation", { name: "Views" });
    const labels = within(nav)
      .getAllByRole("link")
      .map((a) => a.textContent);
    expect(labels.indexOf("Workflows")).toBe(labels.indexOf("Scripts") + 1);
  });

  it("shows only the workflows carrying the labels a grant is limited to", async () => {
    renderApp("/workflows", { world: signedIn("alan@example.net", "org_01ACME") });
    const table = await screen.findByRole("table");
    await waitFor(() => expect(within(table).getAllByRole("row")).toHaveLength(2));
    expect(within(table).getByRole("link", { name: "triage" })).toBeInTheDocument();
  });

  it("is refused without toolsets:read", async () => {
    renderApp("/workflows", { world: signedIn("linus@example.net", "org_01ACME") });
    expect(await screen.findByText(/You cannot see the workflows/)).toBeInTheDocument();
    expect(screen.getByText("toolsets:read")).toBeInTheDocument();
  });
});

describe("WorkflowView", () => {
  const v2 = `triage@${WORKFLOWS.triage.version.slice(0, 12)}`;

  it("shows the versions newest first, each with its lineage and its suite", async () => {
    renderApp("/workflows/triage");
    await versionPanel(v2);
    const panels = screen.getAllByRole("region", { name: /^Version / });
    expect(panels).toHaveLength(3);
    const [latest, second, first] = panels as [HTMLElement, HTMLElement, HTMLElement];
    expect(second).toHaveAccessibleName(`Version ${v2}`);
    expect(within(latest).getByText("latest")).toBeInTheDocument();

    // The latest inherits two tests and adds two, recorded from runs.
    expect(suiteRows(latest)).toEqual([
      expect.stringMatching(/^inheritedcloses nothing without approval/),
      expect.stringMatching(/^inheritedcloses the open issues once approved/),
      expect.stringMatching(/^addedrecorded [0-9a-f-]{36}its run/),
      expect.stringMatching(/^addedrecorded [0-9a-f-]{36}its run/),
    ]);
    const runs = within(latest).getAllByRole("link", { name: "its run" });
    expect(runs[0]?.getAttribute("href")).toMatch(/^\/runs\/[0-9a-f-]{36}$/);
    expect(within(latest).getByRole("button", { name: v2 })).toBeInTheDocument();

    // The second updates one test and retires another, each with its reason.
    const rows = suiteRows(second);
    expect(rows[0]).toMatch(/^inheritedcloses nothing without approval/);
    expect(rows[1]).toMatch(/^updatedcloses the open issues once approved/);
    expect(rows[1]).toContain("Why: only the issues untouched for 30 days are stale");
    expect(rows[2]).toMatch(/^retiredlists the closed issues too/);
    expect(rows[2]).toContain("Why: the workflow never reads closed issues any more");

    // The first derives from nothing: every test is added.
    expect(within(first).getByText("nothing: the first version")).toBeInTheDocument();
    expect(suiteRows(first).every((row) => row.startsWith("added"))).toBe(true);
  });

  it("goes to the parent of a version", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/workflows/triage");
    const latest = await latestPanel();
    expect(await versionPanel(v2)).not.toHaveAttribute("aria-current");
    await user.click(within(latest).getByRole("button", { name: v2 }));
    await waitFor(() => expect(search(router).get("version")).toBe(WORKFLOWS.triage.version));
    expect(await versionPanel(v2)).toHaveAttribute("aria-current", "true");
  });

  it("opens a test on its world, its signals and its expectations", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/workflows/triage");
    const panel = await versionPanel(v2);
    await user.click(
      within(panel).getByRole("button", { name: "closes the open issues once approved" }),
    );
    const test = await screen.findByRole("region", {
      name: "Test closes the open issues once approved",
    });
    await within(test).findByRole("region", { name: "World" });
    expect(search(router).get("test")).not.toBeNull();

    const rules = within(test).getByRole("table", { name: "Tool rules" });
    expect(within(rules).getByText("github.list_issues")).toBeInTheDocument();
    expect(within(rules).getByText("rate limited")).toBeInTheDocument();
    expect(within(rules).getByText("unavailable")).toBeInTheDocument();
    expect(within(rules).getByText("1 call")).toBeInTheDocument();

    const signals = within(test).getByRole("region", { name: "Signals" });
    expect(within(signals).getByText("once the run is blocked")).toBeInTheDocument();
    expect(within(signals).getByText("after 2h")).toBeInTheDocument();

    const expectations = within(test).getByRole("region", { name: "Expectations" });
    expect(within(expectations).getByText("completed")).toBeInTheDocument();
    expect(within(expectations).getByText('{"closed":1}')).toBeInTheDocument();
    expect(within(expectations).getByText("never")).toBeInTheDocument();
    expect(within(expectations).getByText(/listed: the first call/)).toBeInTheDocument();

    await user.click(within(test).getByRole("button", { name: "Close the test" }));
    await waitFor(() => expect(search(router).get("test")).toBeNull());
  });

  it("opens a recorded test on its cassette and the run it comes from", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp("/workflows/triage");
    const latest = await latestPanel();
    const recorded = within(latest).getAllByRole("button", {
      name: /^recorded /,
    })[0] as HTMLElement;
    const runId = (recorded.textContent ?? "").replace("recorded ", "");
    await user.click(recorded);
    const test = await screen.findByRole("region", { name: `Test recorded ${runId}` });
    const cassette = await within(test).findByLabelText("Cassette");
    expect(
      within(cassette).getByText(/3 calls and 1 signal recorded, matched by tool and args/),
    ).toBeInTheDocument();
    expect(within(cassette).getByText("github.close_issue ×2")).toBeInTheDocument();
    // The cassette itself is shown on demand only.
    expect(within(cassette).queryByText(/"by_args"/)).toBeNull();
    expect(within(test).getByText("recorded")).toBeInTheDocument();
    const links = within(test)
      .getAllByRole("link")
      .map((a) => a.getAttribute("href"));
    expect(links).toContain(`/runs/${runId}`);
    expect(within(test).getByText("alice")).toBeInTheDocument();
    // The version it ran is among the versions: a button goes to it.
    expect(within(test).getByRole("button", { name: v2 })).toBeInTheDocument();
    expect(requested("/v1/runs")).toEqual([]);
  });

  it("says plainly when a recorded test cannot be read, and keeps the page", async () => {
    const user = userEvent.setup();
    renderApp("/workflows/triage");
    const latest = await latestPanel();
    const recorded = within(latest).getAllByRole("button", {
      name: /^recorded /,
    })[1] as HTMLElement;
    await user.click(recorded);
    expect(await screen.findByText("You cannot read this test")).toBeInTheDocument();
    expect(screen.getByText(/recorded from a run of the namespace user_9Qx31/)).toBeInTheDocument();
    // The versions are still there.
    expect(await versionPanel(v2)).toBeInTheDocument();
  });

  it("answers a workflow that does not exist", async () => {
    renderApp("/workflows/nope");
    expect(await screen.findByText("The workflow nope could not be read")).toBeInTheDocument();
  });
});
