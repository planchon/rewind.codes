import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { CLAUDE_AI, connect } from "../../mocks/connections";
import { createWorld } from "../../mocks/fixtures";
import { NOW, renderApp, signedIn } from "../../test/app";

/** A step of the page, once the page is shown. */
function step(n: number): Promise<HTMLElement> {
  return screen.findByRole("listitem", { name: new RegExp(`^Step ${n}:`) });
}

describe("Connect Claude", () => {
  it("is in the account menu, not among the views, and lists the toolsets, warning of host tools", async () => {
    const user = userEvent.setup();
    const world = signedIn("grace@example.net");
    renderApp("/", { world });
    const views = await screen.findByRole("navigation", { name: "Views" });
    expect(within(views).queryByRole("link", { name: "Connect Claude" })).toBeNull();
    await user.click(screen.getByLabelText("Account: Grace Hopper"));
    await user.click(await screen.findByRole("menuitem", { name: "Connect Claude" }));
    expect(await screen.findByRole("heading", { name: "Connect Claude" })).toBeInTheDocument();

    const toolsets = await step(1);
    expect(await within(toolsets).findAllByText("github, slack")).toHaveLength(2);
    expect(toolsets).toHaveAccessibleName(/have a toolset \(done\)/i);
    expect(within(toolsets).getByText(/host tools \(ask_user\)/i)).toHaveTextContent(
      "Claude reaches them only while your worker runs",
    );
  });

  it("links to creating a toolset when the organization has none", async () => {
    const world = signedIn("grace@example.net");
    world.connections.toolsets = [];
    renderApp("/connect", { world });
    const toolsets = await step(1);
    const link = await within(toolsets).findByRole("link", { name: "Create a toolset" });
    expect(link).toHaveAttribute("href", "https://docs.rewind.codes/sdk/toolsets");
    expect(toolsets).not.toHaveAccessibleName(/done/);
  });

  it("gives the URL, and the exact steps of each client", async () => {
    const user = userEvent.setup();
    const world = signedIn("grace@example.net");
    renderApp("/connect", { world });
    const connector = await step(2);
    expect(await within(connector).findByText("https://api.rewind.codes/mcp")).toBeInTheDocument();
    expect(within(connector).getByRole("button", { name: "Copy the URL" })).toBeInTheDocument();
    const panel = within(connector).getByRole("tabpanel");
    expect(panel).toHaveTextContent("Customize › Connectors");
    expect(panel).toHaveTextContent("Use Claude's published identity");

    await user.click(within(connector).getByRole("tab", { name: "Team, Enterprise" }));
    expect(within(connector).getByRole("tabpanel")).toHaveTextContent(
      "Organization settings › Connectors › Add › Custom › Web",
    );
    await user.click(within(connector).getByRole("tab", { name: "Claude Code" }));
    expect(
      within(connector).getByText(
        "claude mcp add --transport http rewind https://api.rewind.codes/mcp",
      ),
    ).toBeInTheDocument();
    expect(within(connector).getByRole("tabpanel")).toHaveTextContent("--scope project");
    await user.click(within(connector).getByRole("tab", { name: "Other MCP clients" }));
    expect(within(connector).getByRole("tabpanel")).toHaveTextContent(/register/);
  });

  it("says when MCP is not enabled on the server", async () => {
    const world = signedIn("grace@example.net", undefined, {
      now: NOW,
      mcp: { mcpUrl: null, oauth: false },
    });
    renderApp("/connect", { world });
    expect(
      await within(await step(2)).findByText(/MCP is not enabled on this server/),
    ).toBeVisible();
    expect(within(await step(2)).queryByRole("tablist")).toBeNull();
    expect(await step(4)).toHaveTextContent(/this server makes no connection/i);
  });

  it("gives an API key's command when the server signs no one in for a client", async () => {
    const user = userEvent.setup();
    const world = createWorld({
      now: NOW,
      requireKey: "rewind_sk_good",
      mcp: { mcpUrl: "http://localhost:8080/mcp", oauth: false },
    });
    renderApp("/connect", { world });
    await user.type(await screen.findByLabelText("API key"), "rewind_sk_good");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    const connector = await step(2);
    await user.click(within(connector).getByRole("tab", { name: "Claude Code" }));
    expect(
      within(connector).getByText(
        'claude mcp add --transport http rewind http://localhost:8080/mcp --header "Authorization: Bearer $REWIND_API_KEY"',
      ),
    ).toBeInTheDocument();
    expect(await step(4)).toHaveTextContent(/connections belong to people/i);
  });

  it("sees Claude connect while the page is open", async () => {
    const world = signedIn("grace@example.net");
    // Grace's own connection, made before: the step waits for one that is new or used.
    const { requested } = renderApp("/connect", { world });
    const check = await step(4);
    expect(await within(check).findByText(/waiting for claude to connect/i)).toBeVisible();
    expect(within(check).getByText(/you have 1 connection/i)).toBeInTheDocument();

    // Claude exchanges its code: a new connection.
    connect(world.access, world.connections, {
      userId: "user_01GRACE",
      client: CLAUDE_AI,
      organizations: [{ id: "org_01ACME", access: "run", allowedLabels: { via: "claude" } }],
      lastUsedAt: Date.now(),
    });
    expect(await within(check).findByText("Connected", {}, { timeout: 5_000 })).toBeVisible();
    expect(check).toHaveTextContent(/Claude, used just now/);
    expect(check).toHaveAccessibleName(/done/);
    expect(requested("/v1/connections").length).toBeGreaterThanOrEqual(2);
  });

  it("sees a connection made before the page used since it opened", async () => {
    const world = signedIn("grace@example.net");
    renderApp("/connect", { world });
    const check = await step(4);
    await within(check).findByText(/waiting for claude to connect/i);
    const existing = world.connections.connections.find((c) => c.userId === "user_01GRACE");
    if (!existing) throw new Error("Grace has no connection");
    existing.lastUsedAt = Date.now();
    expect(await within(check).findByText("Connected", {}, { timeout: 5_000 })).toBeVisible();
  });

  it("writes a first message naming the organization, a toolset and a namespace", async () => {
    const user = userEvent.setup();
    const world = signedIn("grace@example.net");
    renderApp("/connect", { world });
    const tryIt = await step(5);
    const toolset = await within(tryIt).findByRole("combobox", { name: "Toolset" });
    await user.selectOptions(toolset, world.connections.toolsets[3]?.id ?? "");
    const namespace = within(tryIt).getByRole("textbox", { name: "Namespace" });
    await user.clear(namespace);
    await user.type(namespace, "alice");
    expect(
      within(tryIt).getByText(
        new RegExp(
          `^With Rewind, org Acme, toolset ${world.connections.toolsets[3]?.id}, namespace alice: `,
        ),
      ),
    ).toBeInTheDocument();
    expect(within(tryIt).getByRole("button", { name: "Copy the message" })).toBeInTheDocument();
  });

  it("lists the person's connections beside the steps, with a link to manage them", async () => {
    const world = signedIn("ada@example.net", "org_01ACME");
    const { router } = renderApp("/connect", { world });
    const side = await screen.findByRole("complementary", {
      name: "Good practices and your connections",
    });
    expect(within(side).getByText(/one connection per person/i)).toBeInTheDocument();
    expect(await within(side).findByText("Claude Code")).toBeInTheDocument();
    expect(within(side).getAllByText("Globex")).toHaveLength(1);
    await userEvent.setup().click(within(side).getByRole("link", { name: /manage/i }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/account/connections"));
  });
});
