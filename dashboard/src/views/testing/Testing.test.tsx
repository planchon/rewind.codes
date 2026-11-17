import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { WORKFLOWS } from "../../mocks/fixtures";
import { renderApp, search, signedIn } from "../../test/app";

const v2 = WORKFLOWS.triage.version;
const v2Ref = `triage@${v2.slice(0, 12)}`;

/** The panel of the version a workflow's page shows. */
async function versionPanel(): Promise<HTMLElement> {
  return screen.findByRole("region", { name: /^Version / });
}

/** The suite's rows of a version's panel: the tag, then the test's name. */
function suiteRows(panel: HTMLElement): string[] {
  return within(panel)
    .getAllByRole("listitem")
    .filter((li) => li.classList.contains("suite-test"))
    .map((li) => li.textContent ?? "");
}

/** The options of a picker of the header, by its label. */
function options(label: string): string[] {
  const picker = screen.getByRole("combobox", { name: label });
  return within(picker)
    .getAllByRole("option")
    .map((o) => o.textContent ?? "");
}

describe("TestingView", () => {
  it("lists each workflow with its latest version and the size of its suite", async () => {
    renderApp("/testing");
    expect(await screen.findByRole("heading", { name: "Testing" })).toBeInTheDocument();
    const table = await screen.findByRole("table");
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows.map((r) => within(r).getAllByRole("link")[0]?.textContent)).toEqual([
      "digest",
      "triage",
    ]);
    const triage = within(rows[1] as HTMLElement);
    expect(triage.getByRole("link", { name: "triage" })).toHaveAttribute("href", "/testing/triage");
    expect(
      triage.getByText("Triage the stale issues of a repository, after approval."),
    ).toBeVisible();
    // The third version: two inherited tests and two recorded ones.
    expect(triage.getByText("4")).toBeInTheDocument();
  });

  it("is in the sidebar as Testing, next to Scripts", async () => {
    renderApp("/testing");
    const nav = await screen.findByRole("navigation", { name: "Views" });
    const labels = within(nav)
      .getAllByRole("link")
      .map((a) => a.textContent);
    expect(labels).not.toContain("Workflows");
    expect(labels.indexOf("Testing")).toBe(labels.indexOf("Scripts") + 1);
  });

  it("shows only the workflows carrying the labels a grant is limited to", async () => {
    renderApp("/testing", { world: signedIn("alan@example.net", "org_01ACME") });
    const table = await screen.findByRole("table");
    await waitFor(() => expect(within(table).getAllByRole("row")).toHaveLength(2));
    expect(within(table).getByRole("link", { name: "triage" })).toBeInTheDocument();
  });

  it("is refused without toolsets:read", async () => {
    renderApp("/testing", { world: signedIn("linus@example.net", "org_01ACME") });
    expect(
      await screen.findByText(/You cannot see the workflows and their tests/),
    ).toBeInTheDocument();
    expect(screen.getByText("toolsets:read")).toBeInTheDocument();
  });
});

describe("WorkflowView", () => {
  it("shows the latest version only, the others a pick away", async () => {
    renderApp("/testing/triage");
    const panel = await versionPanel();
    expect(screen.getAllByRole("region", { name: /^Version / })).toHaveLength(1);
    expect(within(panel).getByText("latest")).toBeInTheDocument();
    expect(options("Version")).toEqual([
      expect.stringMatching(/^triage@[0-9a-f]{12} · latest · 4 tests$/),
      `${v2Ref} · 2 tests`,
      expect.stringMatching(/^triage@[0-9a-f]{12} · 3 tests$/),
    ]);

    // The latest inherits two tests and adds two, recorded from runs.
    expect(suiteRows(panel)).toEqual([
      expect.stringMatching(/^inheritedcloses nothing without approval/),
      expect.stringMatching(/^inheritedcloses the open issues once approved/),
      expect.stringMatching(/^addedrecorded [0-9a-f-]{36}its run/),
      expect.stringMatching(/^addedrecorded [0-9a-f-]{36}its run/),
    ]);
    const runs = within(panel).getAllByRole("link", { name: "its run" });
    expect(runs[0]?.getAttribute("href")).toMatch(/^\/runs\/[0-9a-f-]{36}$/);
  });

  it("shows the version picked, with its updated and retired tests and their reasons", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/testing/triage");
    await versionPanel();
    await user.selectOptions(screen.getByRole("combobox", { name: "Version" }), v2);
    await waitFor(() => expect(search(router).get("version")).toBe(v2));
    const panel = await screen.findByRole("region", { name: `Version ${v2Ref}` });
    expect(within(panel).queryByText("latest")).toBeNull();

    const rows = suiteRows(panel);
    expect(rows[0]).toMatch(/^inheritedcloses nothing without approval/);
    expect(rows[1]).toMatch(/^updatedcloses the open issues once approved/);
    expect(rows[1]).toContain("Why: only the issues untouched for 30 days are stale");
    expect(rows[2]).toMatch(/^retiredlists the closed issues too/);
    expect(rows[2]).toContain("Why: the workflow never reads closed issues any more");

    // Its parent is the first version, which derives from nothing.
    await user.click(within(panel).getByRole("button", { name: /^triage@/ }));
    const first = await screen.findByRole("region", {
      name: `Version triage@${search(router).get("version")?.slice(0, 12)}`,
    });
    expect(within(first).getByText("nothing: the first version")).toBeInTheDocument();
    expect(suiteRows(first).every((row) => row.startsWith("added"))).toBe(true);
  });

  it("opens a test on its own page, in the suite of the version shown", async () => {
    const user = userEvent.setup();
    const { router } = renderApp(`/testing/triage?version=${v2}`);
    const panel = await screen.findByRole("region", { name: `Version ${v2Ref}` });
    await user.click(
      within(panel).getByRole("link", { name: "closes the open issues once approved" }),
    );
    await waitFor(() =>
      expect(router.state.location.pathname).toMatch(/^\/testing\/triage\/tests\/[0-9a-f]+$/),
    );
    expect(search(router).get("version")).toBe(v2);
    expect(
      await screen.findByRole("heading", {
        level: 1,
        name: "closes the open issues once approved",
      }),
    ).toBeInTheDocument();
  });

  it("answers a workflow that does not exist", async () => {
    renderApp("/testing/nope");
    expect(await screen.findByText("The workflow nope could not be read")).toBeInTheDocument();
  });
});

describe("TestView", () => {
  /** The page of the test at `index` of v2's suite, opened from the workflow's page. */
  async function openTest(index: number) {
    const user = userEvent.setup();
    const app = renderApp(`/testing/triage?version=${v2}`);
    const panel = await screen.findByRole("region", { name: `Version ${v2Ref}` });
    const links = within(panel)
      .getAllByRole("link")
      .filter((a) => a.classList.contains("suite-name"));
    await user.click(links[index] as HTMLElement);
    await screen.findByRole("combobox", { name: "Test" });
    return { user, ...app };
  }

  it("shows the test's world, its signals and its expectations", async () => {
    await openTest(1);
    const test = await screen.findByRole("region", {
      name: "Test closes the open issues once approved",
    });
    await within(test).findByRole("region", { name: "World" });

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
  });

  it("says where the test stands in the suite, and why it changed", async () => {
    const { user, router } = await openTest(1);
    expect(screen.getByText("Test 2 of 3 of", { exact: false })).toBeInTheDocument();
    expect(screen.getByText(/Why: only the issues untouched for 30 days are stale/)).toBeVisible();

    // The parent's test it replaced opens in the suite of the parent.
    const replaced = screen.getByTitle("The parent's test");
    await user.click(replaced);
    await waitFor(() => expect(search(router).get("version")).not.toBe(v2));
    expect(await screen.findByText("Test 2 of 3 of", { exact: false })).toBeInTheDocument();
    expect(screen.getByText("in the suite of", { exact: false })).toBeInTheDocument();
  });

  it("goes to another test of the suite by the picker, or to the previous and next ones", async () => {
    const { user, router } = await openTest(0);
    expect(options("Test")).toEqual([
      "closes nothing without approval",
      "closes the open issues once approved",
      "lists the closed issues too",
    ]);
    const path = () => router.state.location.pathname;
    expect(screen.getByRole("button", { name: "Previous test" })).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "Next test" }));
    expect(
      await screen.findByRole("heading", {
        level: 1,
        name: "closes the open issues once approved",
      }),
    ).toBeInTheDocument();
    const second = path();

    // The retired test comes last, with its reason.
    const retired = within(screen.getByRole("combobox", { name: "Test" })).getByRole("option", {
      name: "lists the closed issues too",
    }) as HTMLOptionElement;
    await user.selectOptions(screen.getByRole("combobox", { name: "Test" }), retired.value);
    expect(
      await screen.findByRole("heading", { level: 1, name: "lists the closed issues too" }),
    ).toBeInTheDocument();
    expect(screen.getByText(/Why: the workflow never reads closed issues any more/)).toBeVisible();
    expect(screen.getByRole("button", { name: "Next test" })).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "Previous test" }));
    await waitFor(() => expect(path()).toBe(second));
    expect(search(router).get("version")).toBe(v2);
  });

  it("keeps the test of the same name when another version is picked", async () => {
    const { user, router } = await openTest(1);
    const latest = within(screen.getByRole("combobox", { name: "Version" })).getAllByRole(
      "option",
    )[0] as HTMLOptionElement;
    await user.selectOptions(screen.getByRole("combobox", { name: "Version" }), latest.value);
    await waitFor(() => expect(search(router).get("version")).toBe(latest.value));
    expect(router.state.location.pathname).toMatch(/^\/testing\/triage\/tests\//);
    expect(
      await screen.findByRole("heading", {
        level: 1,
        name: "closes the open issues once approved",
      }),
    ).toBeInTheDocument();
    expect(await screen.findByText("Test 2 of 4 of", { exact: false })).toBeInTheDocument();

    // A recorded test, which the second version does not have: picking it leads to the version.
    await user.click(screen.getByRole("button", { name: "Next test" }));
    await user.click(screen.getByRole("button", { name: "Next test" }));
    expect(await screen.findByText("Test 4 of 4 of", { exact: false })).toBeInTheDocument();
    await user.selectOptions(screen.getByRole("combobox", { name: "Version" }), v2);
    await waitFor(() => expect(router.state.location.pathname).toBe("/testing/triage"));
    expect(search(router).get("version")).toBe(v2);
  });

  it("shows a recorded test with its cassette and the run it comes from", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp("/testing/triage");
    const panel = await versionPanel();
    const recorded = within(panel).getAllByRole("link", { name: /^recorded / })[0] as HTMLElement;
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
    expect(links).toContain(`/testing/triage?version=${v2}`);
    expect(within(test).getByText("alice")).toBeInTheDocument();
    expect(requested("/v1/runs")).toEqual([]);
  });

  it("says plainly when a recorded test cannot be read, and keeps the page", async () => {
    const user = userEvent.setup();
    renderApp("/testing/triage");
    const panel = await versionPanel();
    const recorded = within(panel).getAllByRole("link", { name: /^recorded / })[1] as HTMLElement;
    await user.click(recorded);
    expect(await screen.findByText("You cannot read this test")).toBeInTheDocument();
    expect(screen.getByText(/recorded from a run of the namespace user_9Qx31/)).toBeInTheDocument();
    // The pickers are still there.
    expect(screen.getByRole("combobox", { name: "Test" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Previous test" })).toBeEnabled();
  });

  it("says when the test is not in the suite of the version named", async () => {
    const { router } = renderApp(`/testing/triage?version=${v2}`);
    await screen.findByRole("region", { name: `Version ${v2Ref}` });
    await router.navigate(`/testing/triage/tests/${"0".repeat(64)}?version=${v2}`);
    expect(await screen.findByRole("note")).toHaveTextContent(
      `This test is not in the suite of ${v2Ref}.`,
    );
  });
});
