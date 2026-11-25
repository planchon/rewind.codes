import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { createWorld } from "../../mocks/fixtures";
import { confirmInDialog, NOW, renderApp, signedIn } from "../../test/app";

/** Gives Grace a role of Acme's own, made of these permissions. */
function graceWithRole(name: string, permissions: string[]) {
  const world = signedIn("grace@example.net");
  world.access.roles.get("org_01ACME")?.push({
    name,
    builtIn: false,
    description: `A role for the tests: ${name}.`,
    permissions: permissions as never,
  });
  world.access.grants
    .get("org_01ACME")
    ?.set("member:user_01GRACE", { role: name, allowedLabels: {} });
  return world;
}

/** The row whose first cell holds the text: a member, an email, a key. */
function row(table: HTMLElement, text: string): HTMLElement {
  const found = within(table)
    .getAllByRole("row")
    .find((r) => {
      const first = within(r).queryAllByRole("cell")[0];
      return first ? within(first).queryByText(text) !== null : false;
    });
  if (!found) throw new Error(`no row with ${text}`);
  return found;
}

describe("the Organization view", () => {
  it("shows each part to whoever holds its permission", async () => {
    // Grace's role, harness, reads the members, the roles and the signing keys, and nothing else
    // of it.
    const world = signedIn("grace@example.net");
    renderApp("/organization", { world });
    const nav = await screen.findByRole("navigation", { name: "Organization" });
    expect(
      within(nav)
        .getAllByRole("link")
        .map((a) => a.textContent),
    ).toEqual(["Members", "Invitations", "Roles", "Signing keys", "Connections"]);
    const table = await screen.findByRole("table");
    expect(within(row(table, "Ada Lovelace")).getByText("owner")).toBeInTheDocument();
    expect(within(table).queryByRole("columnheader", { name: "Actions" })).toBeNull();

    await userEvent.setup().click(within(nav).getByRole("link", { name: "Invitations" }));
    // Katherine is a member too: her address is read in the invitations, once they are shown.
    const invitations = await screen.findByRole("region", { name: "Invitations" });
    expect(await within(invitations).findByText("katherine@example.net")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Invite someone" })).toBeNull();
    expect(screen.queryByRole("form", { name: "Invite someone" })).toBeNull();
  });

  it("tells a member without runs:read why the runs are not shown, and where to go", async () => {
    const world = signedIn("linus@example.net");
    const { router } = renderApp("/", { world });
    // His home is what he may open.
    await waitFor(() => expect(router.state.location.pathname).toBe("/organization/members"));
    expect(screen.queryByRole("link", { name: "Runs" })).toBeNull();

    await router.navigate("/runs");
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("You cannot read runs in Acme");
    expect(alert).toHaveTextContent(/your role, auditor, does not give the permission runs:read/i);
    expect(within(alert).getByRole("link", { name: "API keys" })).toBeInTheDocument();
  });

  it("refuses an address the caller may not open", async () => {
    const world = signedIn("grace@example.net");
    renderApp("/organization/api-keys", { world });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      /your role, harness, does not give the permission api_keys:read/i,
    );
  });

  it("creates a role from the catalog, with the permissions' descriptions", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { callsTo } = renderApp("/organization/roles", { world });
    await user.click(await screen.findByRole("button", { name: /new role/i }));
    const editor = screen.getByRole("form", { name: "New role" });
    expect(within(editor).getAllByRole("checkbox")).toHaveLength(26);
    expect(within(editor).getByText("Erase a run, its payloads included.")).toBeInTheDocument();

    const name = within(editor).getByLabelText("Name");
    await user.type(name, "Deployer");
    await user.tab();
    expect(within(editor).getByText(/lowercase letters, digits/i)).toHaveClass("tone-critical");
    await user.clear(name);
    await user.type(name, "deployer");
    await user.type(within(editor).getByLabelText("Description"), "Ships action sets");
    await user.click(within(editor).getByRole("checkbox", { name: /actions:write/ }));
    await user.click(within(editor).getByRole("checkbox", { name: /runs:start/ }));
    await user.click(within(editor).getByRole("button", { name: "Create the role" }));

    const created = await screen.findByRole("listitem", { name: "Role deployer" });
    expect(within(created).getByText("runs:start")).toBeInTheDocument();
    const [put] = callsTo("/v1/roles/deployer", "PUT");
    // In the catalog's order, whatever the order of the clicks.
    expect(JSON.parse((await put?.body) ?? "")).toEqual({
      description: "Ships action sets",
      permissions: ["runs:start", "actions:write"],
    });
  });

  it("offers only the permissions the caller holds", async () => {
    const user = userEvent.setup();
    const world = graceWithRole("role-admin", ["runs:read", "roles:read", "roles:write"]);
    renderApp("/organization/roles", { world });
    // A role giving more than she holds cannot be edited by her.
    const auditor = await screen.findByRole("listitem", { name: "Role auditor" });
    expect(within(auditor).getByRole("button", { name: "Edit the role auditor" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: /new role/i }));
    const editor = screen.getByRole("form", { name: "New role" });
    expect(within(editor).getByRole("checkbox", { name: /runs:read/ })).toBeEnabled();
    expect(within(editor).getByRole("checkbox", { name: /runs:start/ })).toBeDisabled();
  });

  it("explains that a role still given cannot be deleted", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    renderApp("/organization/roles", { world });
    const auditor = await screen.findByRole("listitem", { name: "Role auditor" });
    await user.click(within(auditor).getByRole("button", { name: "Delete" }));
    await confirmInDialog(user);
    // The role goes at once, and comes back with the server's refusal.
    const alert = await waitFor(() =>
      within(screen.getByRole("listitem", { name: "Role auditor" })).getByRole("alert"),
    );
    expect(alert).toHaveTextContent("auditor is still given");
    expect(alert).toHaveTextContent(/give them another role, then delete it/i);
    expect(alert).toHaveTextContent("409 · role-in-use");

    // A role no one holds goes.
    const onCall = screen.getByRole("listitem", { name: "Role on-call" });
    await user.click(within(onCall).getByRole("button", { name: "Delete" }));
    await confirmInDialog(user);
    await waitFor(() =>
      expect(screen.queryByRole("listitem", { name: "Role on-call" })).toBeNull(),
    );
  });

  it("creates a key, shows its secret once, and revokes it", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { callsTo } = renderApp("/organization/api-keys", { world });
    const table = await screen.findByRole("table");
    expect(within(row(table, "triage-agent")).getByText("Grace Hopper")).toBeInTheDocument();
    expect(within(row(table, "old-ci")).getByText(/revoked/)).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /new key/i }));
    const form = screen.getByRole("form", { name: "New API key" });
    await user.type(within(form).getByLabelText("Name"), "deploy-bot");
    await user.selectOptions(within(form).getByLabelText("Role"), "worker");
    await user.type(within(form).getByLabelText("Allowed labels"), "agent:deploy");
    await user.click(within(form).getByRole("button", { name: "Create the key" }));

    const secret = await screen.findByTestId("secret");
    const created = world.access.keys.find((k) => k.name === "deploy-bot");
    expect(secret).toHaveTextContent(created?.secret ?? "missing");
    await user.click(screen.getByRole("button", { name: "Copy" }));
    expect(await navigator.clipboard.readText()).toBe(created?.secret);
    const [post] = callsTo("/v1/api-keys", "POST");
    expect(JSON.parse((await post?.body) ?? "")).toEqual({
      name: "deploy-bot",
      role: "worker",
      allowedLabels: { agent: "deploy" },
    });

    await user.click(screen.getByRole("button", { name: "I have copied it" }));
    expect(screen.queryByTestId("secret")).toBeNull();
    const added = row(screen.getByRole("table"), "deploy-bot");
    expect(within(added).getByText("worker")).toBeInTheDocument();
    expect(within(added).getByText("Ada Lovelace")).toBeInTheDocument();

    await user.click(within(added).getByRole("button", { name: "Revoke" }));
    await confirmInDialog(user);
    await waitFor(() =>
      expect(
        within(row(screen.getByRole("table"), "deploy-bot")).getByText(/revoked/),
      ).toBeVisible(),
    );
  });

  it("offers only the roles the caller may give", async () => {
    const user = userEvent.setup();
    // Reading, and inviting: the roles within that are reader and this one.
    const world = graceWithRole("inviter", [
      "runs:read",
      "actions:read",
      "secrets:read",
      "subscriptions:read",
      "members:read",
      "members:invite",
      "roles:read",
      "namespaces:read",
    ]);
    renderApp("/organization/invitations", { world });
    await user.click(await screen.findByRole("button", { name: "Invite someone" }));
    const form = await screen.findByRole("form", { name: "Invite someone" });
    const options = within(within(form).getByLabelText("Role")).getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual(["reader", "inviter (custom)"]);
    await user.type(within(form).getByLabelText("Email"), "someone@example.net");
    await user.click(within(form).getByRole("button", { name: /send the invitation/i }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "An invitation was sent to someone@example.net.",
    );
  });

  it("explains why the last owner keeps the role", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    renderApp("/organization/members", { world });
    const table = await screen.findByRole("table");
    const ada = await waitFor(() => row(table, "Ada Lovelace"));
    await user.click(within(ada).getByRole("button", { name: /change the role/i }));
    const form = screen.getByRole("form", { name: "Role of Ada Lovelace" });
    await user.selectOptions(within(form).getByLabelText("Role"), "reader");
    await user.click(within(form).getByRole("button", { name: "Save" }));
    const alert = await within(form).findByRole("alert");
    expect(alert).toHaveTextContent("The organization must keep an owner");
    expect(alert).toHaveTextContent(/give the owner role to another member first/i);
  });

  it("changes a member's role and labels, and removes a member", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { callsTo } = renderApp("/organization/members", { world });
    const table = await screen.findByRole("table");
    const grace = await waitFor(() => row(table, "Grace Hopper"));
    await user.click(within(grace).getByRole("button", { name: /change the role/i }));
    const form = screen.getByRole("form", { name: "Role of Grace Hopper" });
    await user.selectOptions(within(form).getByLabelText("Role"), "reader");
    await user.type(within(form).getByLabelText("Allowed labels"), "agent:triage");
    await user.click(within(form).getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(screen.queryByRole("form", { name: "Role of Grace Hopper" })).toBeNull(),
    );
    const [put] = callsTo("/v1/organization/members/user_01GRACE/grant", "PUT");
    expect(JSON.parse((await put?.body) ?? "")).toEqual({
      role: "reader",
      allowedLabels: { agent: "triage" },
    });
    await waitFor(() =>
      expect(within(row(table, "Grace Hopper")).getByText("agent:")).toBeInTheDocument(),
    );

    const linus = row(table, "Linus Torvalds");
    await user.click(within(linus).getByRole("button", { name: "Remove" }));
    await confirmInDialog(user);
    await waitFor(() => expect(within(table).queryByText("Linus Torvalds")).toBeNull());
  });

  it("invites, resends and revokes", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { callsTo } = renderApp("/organization/invitations", { world });
    await user.click(await screen.findByRole("button", { name: "Invite someone" }));
    const form = await screen.findByRole("form", { name: "Invite someone" });
    await user.type(within(form).getByLabelText("Email"), "new@example.net");
    await user.selectOptions(within(form).getByLabelText("Role"), "harness");
    await user.click(within(form).getByRole("button", { name: /send the invitation/i }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "An invitation was sent to new@example.net.",
    );
    const [post] = callsTo("/v1/organization/invitations", "POST");
    expect(JSON.parse((await post?.body) ?? "")).toEqual({
      email: "new@example.net",
      role: "harness",
      allowedLabels: {},
    });

    const table = screen.getByRole("table");
    const dorothy = await waitFor(() => row(table, "dorothy@example.net"));
    expect(within(dorothy).getByText("expired")).toBeInTheDocument();
    await user.click(within(dorothy).getByRole("button", { name: /resend/i }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "The invitation was sent again to dorothy@example.net.",
    );

    const fresh = await waitFor(() => row(table, "new@example.net"));
    await user.click(within(fresh).getByRole("button", { name: "Revoke" }));
    await confirmInDialog(user);
    await waitFor(() =>
      expect(within(row(table, "new@example.net")).getByText("revoked")).toBeInTheDocument(),
    );
  });

  it("renames the organization and sets its default role", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { callsTo } = renderApp("/organization/settings", { world });
    const form = await screen.findByRole("form", { name: "Settings" });
    const name = within(form).getByLabelText("Name");
    await user.clear(name);
    await user.type(name, "Acme Corp");
    await user.selectOptions(within(form).getByLabelText("Role"), "on-call");
    await user.click(within(form).getByRole("button", { name: "Save" }));
    expect(await screen.findByLabelText("Organization: Acme Corp")).toBeInTheDocument();
    const [patch] = callsTo("/v1/organization", "PATCH");
    expect(JSON.parse((await patch?.body) ?? "")).toEqual({
      name: "Acme Corp",
      defaultRole: "on-call",
    });
  });

  it("shows the roles and keys of a key's organization, without WorkOS", async () => {
    // The embedded server: the operator, every permission, no organization.
    renderApp("/organization", { world: createWorld({ now: NOW }) });
    const nav = await screen.findByRole("navigation", { name: "Organization" });
    expect(
      within(nav)
        .getAllByRole("link")
        .map((a) => a.textContent),
    ).toEqual(["Roles", "API keys", "Signing keys"]);
    // Without an organization, its sidebar says access, and the page opens on the first part.
    expect(screen.getByRole("region", { name: "Organization" })).toHaveTextContent(/Access/);
    expect(await screen.findByRole("heading", { name: "Roles" })).toBeInTheDocument();
  });
});

describe("the organization's pages and the namespaces", () => {
  it("show no namespace picker: they are of the organization, whatever the namespace", async () => {
    const world = signedIn("ada@example.net", "org_01ACME");
    renderApp("/organization/api-keys", { world });
    await screen.findByRole("heading", { level: 1 });
    expect(screen.queryByRole("button", { name: /^Namespace: / })).toBeNull();
  });
});
