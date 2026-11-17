import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { SCRIPTS, WORKFLOWS } from "../../mocks/fixtures";
import { renderApp, search, signedIn } from "../../test/app";

const v2 = WORKFLOWS.triage.version;
const v2Ref = `triage@${v2.slice(0, 12)}`;

/** The rows of the suite table, each as its text. */
async function suiteRows(): Promise<string[]> {
  const table = await screen.findByRole("table", { name: "Suite of triage" });
  await waitFor(() => expect(within(table).queryByText("Loading…")).toBeNull());
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.textContent ?? "");
}

/** The tests of the suite in Testing's sidebar. */
function sidebarTests(): string[] {
  const nav = screen.getByRole("navigation", { name: "Tests" });
  return within(nav)
    .getAllByRole("link")
    .map((a) => a.textContent ?? "");
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
    renderApp("/scripts");
    const nav = await screen.findByRole("navigation", { name: "Views" });
    const labels = within(nav)
      .getAllByRole("link")
      .map((a) => a.textContent);
    expect(labels).not.toContain("Workflows");
    expect(labels.indexOf("Testing")).toBe(labels.indexOf("Scripts") + 1);
  });

  it("has a sidebar of its own, with the workflows", async () => {
    renderApp("/testing");
    const pane = await screen.findByRole("region", { name: "Testing" });
    const workflows = within(pane).getByRole("navigation", { name: "Workflows" });
    await waitFor(() =>
      expect(
        within(workflows)
          .getAllByRole("link")
          .map((a) => a.textContent),
      ).toEqual(["digest1", "triage4"]),
    );
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
  it("shows the latest version, its interface and its suite as a table", async () => {
    renderApp("/testing/triage");
    const title = await screen.findByRole("heading", { level: 1 });
    await within(title).findByText("latest");
    expect(await suiteRows()).toEqual([
      expect.stringMatching(/^closes nothing without approvalinherited/),
      expect.stringMatching(/^closes the open issues once approvedinherited/),
      expect.stringMatching(/^recorded [0-9a-f-]{36}added recorded\{"repo"/),
      expect.stringMatching(/^recorded [0-9a-f-]{36}added recorded You cannot read this test/),
    ]);
    const table = screen.getByRole("table", { name: "Suite of triage" });
    // Each test's input, and the output it expects, open in a dialog.
    expect(within(table).getAllByRole("button", { name: /: input$/ }).length).toBeGreaterThan(0);
    const recorded = within(table).getAllByRole("link", { name: "recorded" })[0];
    expect(recorded?.getAttribute("href")).toMatch(/^\/runs\/[0-9a-f-]{36}$/);
    // A recorded test the caller may not read says so in its row.
    expect(within(table).getByText(/You cannot read this test/)).toBeInTheDocument();

    // Its suite and its details in the sidebar.
    expect(sidebarTests()).toEqual([
      "All its tests4",
      "closes nothing without approval",
      "closes the open issues once approved",
      expect.stringMatching(/^recorded /),
      expect.stringMatching(/^recorded /),
    ]);
    const details = screen.getByRole("region", { name: "Details" });
    expect(within(details).getByRole("link", { name: v2.slice(0, 12) })).toHaveAttribute(
      "href",
      `/testing/triage?version=${v2}`,
    );
  });

  it("opens a schema of its interface in a dialog, colored", async () => {
    const user = userEvent.setup();
    renderApp("/testing/triage");
    const iface = await screen.findByRole("region", { name: "Interface" });
    await user.click(within(iface).getByRole("button", { name: /input schema$/ }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getAllByText('"repo":')[0]).toHaveClass("j-key");
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("picks another version in the breadcrumb, with its updated and retired tests", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/testing/triage");
    await user.click(await screen.findByTitle("Version: pick another"));
    const versions = await screen.findAllByRole("menuitem");
    expect(versions.map((v) => v.textContent)).toEqual([
      expect.stringMatching(/^[0-9a-f]{12}latest · 4 tests$/),
      `${v2.slice(0, 12)}2 tests`,
      expect.stringMatching(/^[0-9a-f]{12}3 tests$/),
    ]);
    await user.click(versions[1] as HTMLElement);
    await waitFor(() => expect(search(router).get("version")).toBe(v2));

    const rows = await suiteRows();
    expect(rows[0]).toMatch(/^closes nothing without approvalinherited/);
    expect(rows[1]).toMatch(/^closes the open issues once approvedreplaces [0-9a-f]{12}/);
    expect(rows[1]).toContain("Why: only the issues untouched for 30 days are stale");
    const retired = screen.getByRole("table", { name: "Retired from the suite of triage" });
    expect(within(retired).getByText("lists the closed issues too")).toBeInTheDocument();
    expect(
      within(retired).getByText(/the workflow never reads closed issues any more/),
    ).toBeInTheDocument();
  });

  it("opens a test on its own page, in the suite of the version shown", async () => {
    const user = userEvent.setup();
    const { router } = renderApp(`/testing/triage?version=${v2}`);
    const table = await screen.findByRole("table", { name: "Suite of triage" });
    await user.click(
      within(table).getByRole("link", { name: "closes the open issues once approved" }),
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
  /** The page of the test at `index` of v2's suite, opened from the sidebar. */
  async function openTest(index: number) {
    const user = userEvent.setup();
    const app = renderApp(`/testing/triage?version=${v2}`);
    await screen.findByRole("table", { name: "Suite of triage" });
    const nav = screen.getByRole("navigation", { name: "Tests" });
    await user.click(within(nav).getAllByRole("link")[index + 1] as HTMLElement);
    await screen.findByRole("region", { name: /^Test / });
    return { user, ...app };
  }

  it("shows its input and expected output, its world, its signals and its expectations", async () => {
    const { user } = await openTest(1);
    const test = screen.getByRole("region", { name: "Test closes the open issues once approved" });
    expect(within(test).getByRole("region", { name: "Input" })).toBeInTheDocument();
    const output = within(test).getByRole("region", { name: "Expected output" });
    expect(within(output).getByText('"closed":')).toHaveClass("j-key");

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
    expect(within(expectations).getByText("never")).toBeInTheDocument();
    expect(within(expectations).getByText(/listed: the first call/)).toBeInTheDocument();

    // Every value opens whole in a dialog; so does the whole document.
    await user.click(within(test).getByRole("button", { name: "Open Input" }));
    const input = await screen.findByRole("dialog");
    expect(within(input).getByText("Input")).toBeInTheDocument();
    expect(within(input).getByText('"repo":')).toHaveClass("j-key");
    await user.click(within(input).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await user.click(screen.getByRole("button", { name: "JSON" }));
    const whole = await screen.findByRole("dialog");
    expect(within(whole).getByText('"expect":')).toHaveClass("j-key");

    // Its objects and arrays fold, one by one or all at once.
    expect(within(whole).getAllByText('"closed":').length).toBeGreaterThan(0);
    await user.click(within(whole).getByRole("button", { name: "Fold all" }));
    expect(within(whole).queryByText('"closed":')).toBeNull();
    expect(within(whole).getByText('"expect":')).toBeInTheDocument();
    await user.click(within(whole).getByRole("button", { name: "Unfold all" }));
    expect(within(whole).getAllByText('"closed":').length).toBeGreaterThan(0);
    const expect_ = within(whole)
      .getByText('"expect":')
      .closest('[role="treeitem"]') as HTMLElement;
    await user.click(within(expect_).getAllByRole("button", { name: "Fold" })[0] as HTMLElement);
    expect(within(whole).queryByText('"closed":')).toBeNull();
    expect(within(whole).getByText('"input":')).toBeInTheDocument();
  });

  it("says where the test stands in the suite, and why it changed", async () => {
    const { user, router } = await openTest(1);
    expect(screen.getByText("Test 2 of 3 of", { exact: false })).toBeInTheDocument();
    expect(screen.getByText(/Why: only the issues untouched for 30 days are stale/)).toBeVisible();

    // The parent's test it replaced opens in the suite of the parent.
    await user.click(screen.getByTitle("The parent's test"));
    await waitFor(() => expect(search(router).get("version")).not.toBe(v2));
    expect(await screen.findByText("Test 2 of 3 of", { exact: false })).toBeInTheDocument();
    expect(screen.getByText("in the suite of", { exact: false })).toBeInTheDocument();
  });

  it("goes to another test by the breadcrumb, or to the previous and next ones", async () => {
    const { user, router } = await openTest(0);
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
    // The sidebar marks the test shown.
    const nav = screen.getByRole("navigation", { name: "Tests" });
    expect(within(nav).getByRole("link", { current: "page" })).toHaveTextContent(
      "closes the open issues once approved",
    );

    // The breadcrumb lists the suite, the retired tests last.
    await user.click(within(screen.getByRole("heading", { level: 1 })).getByRole("button"));
    const tests = await screen.findAllByRole("menuitem");
    expect(tests.map((t) => t.textContent)).toEqual([
      "closes nothing without approval",
      "closes the open issues once approved",
      "lists the closed issues too",
    ]);
    await user.click(tests[2] as HTMLElement);
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
    await user.click(screen.getByTitle("Version: pick another"));
    const latest = (await screen.findAllByRole("menuitem"))[0] as HTMLElement;
    await user.click(latest);
    await waitFor(() => expect(search(router).get("version")).not.toBe(v2));
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
    await user.click(screen.getByTitle("Version: pick another"));
    await user.click((await screen.findAllByRole("menuitem"))[1] as HTMLElement);
    await waitFor(() => expect(router.state.location.pathname).toBe("/testing/triage"));
    expect(search(router).get("version")).toBe(v2);
  });

  it("shows a recorded test with its cassette and the run it comes from", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp("/testing/triage");
    await screen.findByRole("table", { name: "Suite of triage" });
    const nav = screen.getByRole("navigation", { name: "Tests" });
    const recorded = within(nav).getAllByRole("link", { name: /^recorded / })[0] as HTMLElement;
    const runId = (recorded.textContent ?? "").replace("recorded ", "");
    await user.click(recorded);
    const test = await screen.findByRole("region", { name: `Test recorded ${runId}` });
    const cassette = await within(test).findByLabelText("Cassette");
    expect(
      within(cassette).getByText(/3 calls and 1 signal recorded, matched by tool and args/),
    ).toBeInTheDocument();
    expect(within(cassette).getByText("github.close_issue ×2")).toBeInTheDocument();
    await user.click(within(cassette).getByRole("button", { name: "Open the cassette" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText('"by_args"')).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

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
    await screen.findByRole("table", { name: "Suite of triage" });
    const nav = screen.getByRole("navigation", { name: "Tests" });
    await user.click(within(nav).getAllByRole("link", { name: /^recorded / })[1] as HTMLElement);
    expect(await screen.findByText("You cannot read this test")).toBeInTheDocument();
    expect(screen.getByText(/recorded from a run of the namespace user_9Qx31/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Previous test" })).toBeEnabled();
    expect(screen.queryByRole("button", { name: "JSON" })).toBeNull();
  });

  it("says when the test is not in the suite of the version named", async () => {
    const { router } = renderApp(`/testing/triage?version=${v2}`);
    await screen.findByRole("table", { name: "Suite of triage" });
    await router.navigate(`/testing/triage/tests/${"0".repeat(64)}?version=${v2}`);
    expect(await screen.findByText(/This test is not in the suite of/)).toHaveTextContent(
      `This test is not in the suite of ${v2Ref}.`,
    );
  });
});

describe("ScriptView", () => {
  it("shows the suite of the workflow versions the script belongs to", async () => {
    const { requested } = renderApp(`/scripts/${SCRIPTS.TRIAGE_REPO.digest}`);
    const section = await screen.findByRole("region", { name: "Tests of this script" });
    const suite = await within(section).findByRole("region", { name: /^Suite of triage@/ });
    expect(await within(suite).findByText("latest")).toBeInTheDocument();
    expect(within(suite).getByText(/also the script of 2 older versions/)).toBeInTheDocument();
    expect(await within(suite).findByRole("table", { name: "Suite of triage" })).toBeVisible();
    expect(
      requested("/v1/workflows").some(
        (u) => u.searchParams.get("script") === SCRIPTS.TRIAGE_REPO.digest,
      ),
    ).toBe(true);
  });

  it("says a script no workflow version has keeps no tests", async () => {
    renderApp(`/scripts/${SCRIPTS.FORECAST.digest}`);
    expect(await screen.findByText(/No workflow version has this script/)).toBeInTheDocument();
  });
});
