import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { confirmInDialog, renderApp, signedIn } from "../../test/app";

function row(table: HTMLElement, person: string): HTMLElement {
  const found = within(table)
    .getAllByRole("row")
    .filter((r) => within(r).queryAllByRole("cell")[0]?.textContent?.includes(person));
  if (found.length === 0) throw new Error(`no row for ${person}`);
  return found[0] as HTMLElement;
}

describe("Organization › Connections", () => {
  it("shows the connections that open the organization, read-only with members:read", async () => {
    // Grace's role, harness, reads the members but does not manage them.
    const world = signedIn("grace@example.net");
    renderApp("/organization/connections", { world });
    const nav = await screen.findByRole("navigation", { name: "Organization" });
    expect(within(nav).getByRole("link", { name: "Connections" })).toBeInTheDocument();

    const table = await screen.findByRole("table");
    // Ada's two (claude.ai and Claude Code) and Grace's own.
    expect(within(table).getAllByRole("row")).toHaveLength(4);
    const ada = row(table, "Ada Lovelace");
    expect(within(ada).getByText("Run")).toBeInTheDocument();
    // Only Acme's access shows: Ada's Globex is not this organization's business.
    expect(within(table).queryByText("Globex")).toBeNull();
    expect(within(row(table, "Grace Hopper")).getByText("you")).toBeInTheDocument();
    expect(within(table).queryByRole("columnheader", { name: "Actions" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Remove from organization" })).toBeNull();
  });

  it("lets whoever holds members:manage remove the organization from a connection", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { callsTo } = renderApp("/organization/connections", { world });
    const table = await screen.findByRole("table");
    const grace = row(table, "Grace Hopper");
    const id = world.connections.connections.find((c) => c.userId === "user_01GRACE")?.id;
    await user.click(within(grace).getByRole("button", { name: "Remove from organization" }));
    const dialog = await screen.findByRole("dialog", {
      name: "Remove Acme from Grace Hopper's Claude?",
    });
    const confirm = within(dialog).getByRole("button", { name: "Remove" });
    expect(confirm).toHaveClass("danger");
    await user.click(confirm);

    await waitFor(() =>
      expect(callsTo(`/v1/organization/connections/${id}`, "DELETE")).toHaveLength(1),
    );
    await waitFor(() =>
      expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(3),
    );
    expect(world.access.grants.get("org_01ACME")?.has(`connection:${id}`)).toBe(false);
  });

  it("drops a member's connections with the member", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { router } = renderApp("/organization/members", { world });
    const members = await screen.findByRole("table");
    const grace = row(members, "Grace Hopper");
    await user.click(within(grace).getByRole("button", { name: "Remove" }));
    await confirmInDialog(user);
    await waitFor(() =>
      expect(within(screen.getByRole("table")).queryByText("Grace Hopper")).toBeNull(),
    );

    await router.navigate("/organization/connections");
    const region = await screen.findByRole("region", { name: "Connections" });
    const table = await within(region).findByRole("table");
    await waitFor(() => expect(within(table).getAllByRole("row")).toHaveLength(3));
    expect(within(table).queryByText("Grace Hopper")).toBeNull();
  });

  it("is refused without members:read", async () => {
    const world = signedIn("grace@example.net");
    world.access.roles.get("org_01ACME")?.push({
      name: "runner",
      builtIn: false,
      description: "Runs only.",
      permissions: ["runs:read", "runs:start"],
    });
    world.access.grants
      .get("org_01ACME")
      ?.set("user:user_01GRACE", { role: "runner", allowedLabels: {} });
    renderApp("/organization/connections", { world });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      /your role, runner, does not give the permission members:read/i,
    );
  });
});
