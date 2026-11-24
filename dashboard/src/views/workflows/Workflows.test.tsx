import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { WORKFLOWS } from "../../mocks/fixtures";
import { renderApp, search, signedIn } from "../../test/app";

const triage = WORKFLOWS.triage;
const page = `/workflows/${triage}`;

/** The rows of the suite table of version `n` of triage, each as its text. */
async function suiteRows(n: number): Promise<string[]> {
  const table = await screen.findByRole("table", { name: `Suite of triage v${n}` });
  await waitFor(() => expect(within(table).queryByText("Loading…")).toBeNull());
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.textContent ?? "");
}

/** The tests of the suite in the workflow's sidebar. */
function sidebarTests(): string[] {
  const nav = screen.getByRole("navigation", { name: "Tests" });
  return within(nav)
    .getAllByRole("link")
    .map((a) => a.textContent ?? "");
}

describe("WorkflowView", () => {
  it("shows the latest version: its code, its interface and its suite as a table", async () => {
    renderApp(page);
    const title = await screen.findByRole("heading", { level: 1 });
    await within(title).findByText("latest");
    expect(title).toHaveTextContent("v3");
    expect(await screen.findByRole("region", { name: "Code" })).toHaveTextContent(
      /workflow\.ts · \d+ lines/,
    );
    expect(await suiteRows(3)).toEqual([
      expect.stringMatching(/^closes nothing without approvalinherited/),
      expect.stringMatching(/^closes the open issues once approvedinherited/),
      expect.stringMatching(/^recorded [0-9a-f-]{36}added recorded\{"repo"/),
      expect.stringMatching(/^recorded [0-9a-f-]{36}added recorded You cannot read this test/),
    ]);
    const table = screen.getByRole("table", { name: "Suite of triage v3" });
    // Each test's input, and the output it expects, open in a dialog.
    expect(within(table).getAllByRole("button", { name: /: input$/ }).length).toBeGreaterThan(0);
    const recorded = within(table).getAllByRole("link", { name: "recorded" })[0];
    expect(recorded?.getAttribute("href")).toMatch(/^\/runs\/[0-9a-f-]{36}$/);

    // Its versions, its suite and its details in its own sidebar.
    const pane = screen.getByRole("region", { name: "triage" });
    const versions = within(pane).getByRole("navigation", { name: "Versions" });
    expect(
      within(versions)
        .getAllByRole("link")
        .map((a) => a.textContent),
    ).toEqual(["v3 latest4", "v22", "v13"]);
    expect(sidebarTests()).toEqual([
      "All its tests4",
      "closes nothing without approval",
      "closes the open issues once approved",
      expect.stringMatching(/^recorded /),
      expect.stringMatching(/^recorded /),
    ]);
    const details = screen.getByRole("region", { name: "Details" });
    expect(within(details).getByText("Organization")).toBeInTheDocument();
    expect(within(details).getByRole("link", { name: "v2" })).toHaveAttribute(
      "href",
      `${page}?version=2`,
    );
  });

  it("opens a schema of its interface in a dialog, colored", async () => {
    const user = userEvent.setup();
    renderApp(page);
    const iface = await screen.findByRole("region", { name: "Interface" });
    await user.click(within(iface).getByRole("button", { name: /input schema$/ }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getAllByText('"repo":')[0]).toHaveClass("j-key");
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("picks another version in the breadcrumb, with its replaced and retired tests", async () => {
    const user = userEvent.setup();
    const { router } = renderApp(page);
    await user.click(await screen.findByTitle("Version: pick another"));
    const versions = await screen.findAllByRole("menuitem");
    expect(versions.map((v) => v.textContent)).toEqual([
      "v3latest · 4 tests",
      "v22 tests",
      "v13 tests",
    ]);
    await user.click(versions[1] as HTMLElement);
    await waitFor(() => expect(search(router).get("version")).toBe("2"));

    const rows = await suiteRows(2);
    expect(rows[0]).toMatch(/^closes nothing without approvalinherited/);
    expect(rows[1]).toMatch(/^closes the open issues once approvedreplaces [0-9a-f]{12}/);
    expect(rows[1]).toContain("Why: only the issues untouched for 30 days are stale");
    const retired = screen.getByRole("table", { name: "Retired from the suite of triage v2" });
    expect(within(retired).getByText("lists the closed issues too")).toBeInTheDocument();
    expect(
      within(retired).getByText(/the workflow never reads closed issues any more/),
    ).toBeInTheDocument();
  });

  it("opens a test on its own page, in the suite of the version shown", async () => {
    const user = userEvent.setup();
    const { router } = renderApp(`${page}?version=2`);
    const table = await screen.findByRole("table", { name: "Suite of triage v2" });
    await user.click(
      within(table).getByRole("link", { name: "closes the open issues once approved" }),
    );
    await waitFor(() =>
      expect(router.state.location.pathname).toMatch(
        new RegExp(`^/workflows/${triage}/tests/[0-9a-f]+$`),
      ),
    );
    expect(search(router).get("version")).toBe("2");
    expect(
      await screen.findByRole("heading", {
        level: 1,
        name: "closes the open issues once approved",
      }),
    ).toBeInTheDocument();
  });

  it("shows a workflow without a name by its short id, in its namespace", async () => {
    renderApp(`/workflows/${WORKFLOWS.chenDigest}`);
    const pane = await screen.findByRole("region", {
      name: `wf-${WORKFLOWS.chenDigest.slice(3, 9)}`,
    });
    const details = within(pane).getByRole("region", { name: "Details" });
    expect(await within(details).findByText("Chen Logistics")).toBeInTheDocument();
    expect(await screen.findByText("No test: this version's suite is empty.")).toBeVisible();
    // No interface: it accepts any input, output and signal.
    expect(screen.queryByRole("region", { name: "Interface" })).toBeNull();
  });

  it("names its page by its id and its version's number, from a link by name and digest", async () => {
    const first = renderApp(`${page}?version=2`);
    await screen.findByRole("table", { name: "Suite of triage v2" });
    const v2 = first.world.workflows.workflows.find((w) => w.id === triage)?.versions[1]?.digest;
    first.unmount();
    // Testing's link named the workflow by its name, the version by its digest.
    const { router, requested } = renderApp(`/testing/triage?version=${v2}`);
    await waitFor(() => expect(router.state.location.pathname).toBe(page));
    await waitFor(() => expect(search(router).get("version")).toBe("2"));
    expect(requested("/v1/workflows/triage").length).toBeGreaterThan(0);
    expect(await screen.findByRole("table", { name: "Suite of triage v2" })).toBeVisible();
  });

  it("answers a workflow that does not exist", async () => {
    renderApp("/workflows/nope");
    expect(await screen.findByText("nope could not be read")).toBeInTheDocument();
  });

  it("is refused without runs:read", async () => {
    renderApp(page, { world: signedIn("linus@example.net", "org_01ACME") });
    expect(await screen.findByText(/runs:read/)).toBeInTheDocument();
  });
});

describe("a workflow's controls", () => {
  it("names and describes it, and says a name its scope has under the field", async () => {
    const user = userEvent.setup();
    const { callsTo } = renderApp(`/workflows/${WORKFLOWS.forecast}`);
    const title = await screen.findByRole("region", {
      name: "One-shot lookup started by the support agent.",
    });
    expect(title).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Edit" }));
    const form = await screen.findByRole("form", { name: "Name and description" });
    const name = within(form).getByLabelText("Name");
    await user.type(name, "digest");
    await user.click(within(form).getByRole("button", { name: "Save" }));
    expect(await within(form).findByText(/already has a workflow named digest/)).toHaveClass(
      "tone-critical",
    );
    await user.clear(name);
    await user.type(name, "weather");
    await user.type(within(form).getByLabelText("Description"), "The forecast for a city.");
    await user.click(within(form).getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(screen.queryByRole("form", { name: "Name and description" })).toBeNull(),
    );
    const patches = callsTo(`/v1/workflows/${WORKFLOWS.forecast}`, "PATCH");
    expect(JSON.parse((await patches.at(-1)?.body) ?? "")).toEqual({
      name: "weather",
      description: "The forecast for a city.",
    });
    // The page and its sidebar show its name at once.
    expect(await screen.findByRole("region", { name: "weather" })).toBeInTheDocument();
  });

  it("promotes a namespace's workflow to the organization", async () => {
    const user = userEvent.setup();
    renderApp(`/workflows/${WORKFLOWS.staleIssues}`);
    const details = await screen.findByRole("region", { name: "Details" });
    expect(within(details).getByText("alice")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Fork" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Promote" }));
    const form = await screen.findByRole("form", { name: "Promote triage_stale_issues?" });
    await user.click(within(form).getByRole("button", { name: "Promote" }));
    await waitFor(() =>
      expect(
        within(screen.getByRole("region", { name: "Details" })).getByText("Organization"),
      ).toBeInTheDocument(),
    );
    expect(screen.queryByRole("button", { name: "Promote" })).toBeNull();
    expect(screen.getByRole("button", { name: "Fork" })).toBeInTheDocument();
  });

  it("forks an organization's workflow into a namespace picked in the dialog", async () => {
    const user = userEvent.setup();
    const { router, callsTo } = renderApp(`/workflows/${WORKFLOWS.digest}`);
    await screen.findByRole("table", { name: "Suite of digest v1" });
    expect(screen.queryByRole("button", { name: "Promote" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Fork" }));
    const form = await screen.findByRole("form", { name: "Fork digest" });
    expect(within(form).getByRole("button", { name: "Fork" })).toBeDisabled();
    await user.type(within(form).getByRole("textbox", { name: "Find a namespace" }), "bo");
    await user.click(await within(form).findByRole("radio", { name: /^bob/ }));
    await user.click(within(form).getByRole("button", { name: "Fork" }));
    await waitFor(() =>
      expect(router.state.location.pathname).not.toBe(`/workflows/${WORKFLOWS.digest}`),
    );
    const [post] = callsTo(`/v1/workflows/${WORKFLOWS.digest}/fork`, "POST");
    expect(post?.headers.get("Rewind-Namespace")).toBe("bob");
    // The fork, in bob's namespace, says its source.
    expect(await screen.findByRole("link", { name: "digest v1" })).toHaveAttribute(
      "href",
      `/workflows/${WORKFLOWS.digest}?version=1`,
    );
    const details = screen.getByRole("region", { name: "Details" });
    expect(within(details).getByText("bob")).toBeInTheDocument();
  });

  it("pulls into a fork its source's latest version", async () => {
    const user = userEvent.setup();
    renderApp(`/workflows/${WORKFLOWS.triageFork}`);
    expect(await screen.findByRole("link", { name: "triage v2" })).toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: "Pull v3" }));
    const form = await screen.findByRole("form", { name: "Pull triage v3?" });
    expect(form).toHaveTextContent("becomes this fork's next version once its own suite passes");
    await user.click(within(form).getByRole("button", { name: "Pull" }));
    await waitFor(() =>
      expect(
        within(screen.getByRole("region", { name: "Details" })).getByRole("link", {
          name: "triage v3",
        }),
      ).toBeInTheDocument(),
    );
    const title = screen.getByRole("heading", { level: 1 });
    await waitFor(() => expect(title).toHaveTextContent("v2"));
    expect(screen.queryByRole("button", { name: /^Pull/ })).toBeNull();
  });

  it("shows no control without workflows:write", async () => {
    renderApp(page, { world: signedIn("alan@example.net", "org_01ACME") });
    await screen.findByRole("table", { name: "Suite of triage v3" });
    expect(screen.queryByRole("button", { name: "Edit" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Fork" })).toBeNull();
  });
});

describe("TestView", () => {
  /** The page of the test at `index` of v2's suite, opened from the sidebar. */
  async function openTest(index: number) {
    const user = userEvent.setup();
    const app = renderApp(`${page}?version=2`);
    await screen.findByRole("table", { name: "Suite of triage v2" });
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

    const rules = within(test).getByRole("table", { name: "Rules" });
    expect(within(rules).getByText("github.list_issues")).toBeInTheDocument();
    expect(within(rules).getByText("rate limited")).toBeInTheDocument();
    expect(within(rules).getByText("unavailable")).toBeInTheDocument();
    expect(within(rules).getByText("1 call")).toBeInTheDocument();

    const signals = within(test).getByRole("region", { name: "Signals" });
    expect(within(signals).getByText("once the run waits")).toBeInTheDocument();
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
    expect(screen.getByText(/test 2 of 3 in the suite of/)).toBeInTheDocument();
    expect(screen.getByText(/Why: only the issues untouched for 30 days are stale/)).toBeVisible();

    // The base's test it replaced opens in the suite of the base.
    await user.click(screen.getByTitle("The test of the same name in v1"));
    await waitFor(() => expect(search(router).get("version")).toBe("1"));
    expect(await screen.findByText(/test 2 of 3 in the suite of/)).toBeInTheDocument();
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
    expect(search(router).get("version")).toBe("2");
  });

  it("keeps the test of the same name when another version is picked", async () => {
    const { user, router } = await openTest(1);
    await user.click(screen.getByTitle("Version: pick another"));
    const latest = (await screen.findAllByRole("menuitem"))[0] as HTMLElement;
    await user.click(latest);
    await waitFor(() => expect(search(router).get("version")).toBeNull());
    expect(router.state.location.pathname).toMatch(new RegExp(`^/workflows/${triage}/tests/`));
    expect(
      await screen.findByRole("heading", {
        level: 1,
        name: "closes the open issues once approved",
      }),
    ).toBeInTheDocument();
    expect(await screen.findByText(/test 2 of 4 in the suite of/)).toBeInTheDocument();

    // A recorded test, which the second version does not have: picking it leads to the version.
    await user.click(screen.getByRole("button", { name: "Next test" }));
    await user.click(screen.getByRole("button", { name: "Next test" }));
    expect(await screen.findByText(/test 4 of 4 in the suite of/)).toBeInTheDocument();
    await user.click(screen.getByTitle("Version: pick another"));
    await user.click((await screen.findAllByRole("menuitem"))[1] as HTMLElement);
    await waitFor(() => expect(router.state.location.pathname).toBe(page));
    expect(search(router).get("version")).toBe("2");
  });

  it("shows a recorded test with its cassette and the run it comes from", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp(page);
    await screen.findByRole("table", { name: "Suite of triage v3" });
    const nav = screen.getByRole("navigation", { name: "Tests" });
    const recorded = within(nav).getAllByRole("link", { name: /^recorded / })[0] as HTMLElement;
    const runId = (recorded.textContent ?? "").replace("recorded ", "");
    await user.click(recorded);
    const test = await screen.findByRole("region", { name: `Test recorded ${runId}` });
    const cassette = await within(test).findByLabelText("Cassette");
    expect(
      within(cassette).getByText(/3 calls and 1 signal recorded, matched by action and args/),
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
    expect(links).toContain(`${page}?version=2`);
    expect(within(test).getByText("triage v2")).toBeInTheDocument();
    expect(within(test).getByText("alice")).toBeInTheDocument();
    expect(requested("/v1/runs")).toEqual([]);
  });

  it("says plainly when a recorded test cannot be read, and keeps the page", async () => {
    const user = userEvent.setup();
    renderApp(page);
    await screen.findByRole("table", { name: "Suite of triage v3" });
    const nav = screen.getByRole("navigation", { name: "Tests" });
    await user.click(within(nav).getAllByRole("link", { name: /^recorded / })[1] as HTMLElement);
    expect(await screen.findByText("You cannot read this test")).toBeInTheDocument();
    expect(screen.getByText(/recorded from a run of the namespace user_9Qx31/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Previous test" })).toBeEnabled();
    expect(screen.queryByRole("button", { name: "JSON" })).toBeNull();
  });

  it("says when the test is not in the suite of the version named", async () => {
    const { router } = renderApp(`${page}?version=2`);
    await screen.findByRole("table", { name: "Suite of triage v2" });
    await router.navigate(`${page}/tests/${"0".repeat(64)}?version=2`);
    expect(await screen.findByText(/This test is not in the suite of/)).toHaveTextContent(
      "This test is not in the suite of triage v2.",
    );
  });

  it("opens from an old link of Testing", async () => {
    const { router } = renderApp(`/testing/triage/tests/${"0".repeat(64)}`);
    await waitFor(() =>
      expect(router.state.location.pathname).toBe(`${page}/tests/${"0".repeat(64)}`),
    );
  });
});
