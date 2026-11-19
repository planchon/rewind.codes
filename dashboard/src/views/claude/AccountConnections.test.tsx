import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { renderApp, signedIn } from "../../test/app";

/** The row of a connection, by its client's name. */
function row(table: HTMLElement, client: string): HTMLElement {
  const found = within(table)
    .getAllByRole("row")
    .find((r) => within(r).queryAllByRole("cell")[0]?.textContent?.startsWith(client));
  if (!found) throw new Error(`no row for ${client}`);
  return found;
}

describe("Account › Connections", () => {
  it("opens from the account menu and lists the person's connections", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { router } = renderApp("/overview", { world });
    await user.click(await screen.findByRole("button", { name: "Account: Ada Lovelace" }));
    await user.click(await screen.findByRole("menuitem", { name: "Connections" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/account/connections"));

    const table = await screen.findByRole("table");
    const claude = row(table, "Claude");
    expect(within(claude).getByText("claude.ai")).toBeInTheDocument();
    expect(within(claude).getByText("Acme")).toBeInTheDocument();
    expect(within(claude).getByText("Globex")).toBeInTheDocument();
    expect(within(claude).getByText("Run")).toBeInTheDocument();
    expect(within(claude).getByText("Read")).toBeInTheDocument();
    expect(within(claude).getAllByText("via:")).toHaveLength(2);
    expect(within(claude).getByText("2h ago")).toBeInTheDocument();
    expect(row(table, "Claude Code")).toHaveTextContent("1d ago");
  });

  it("changes what a connection opens in a dialog", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { callsTo } = renderApp("/account/connections", { world });
    const table = await screen.findByRole("table");
    const id = world.connections.connections.find(
      (c) => c.userId === "user_01ADA" && c.client.name === "Claude",
    )?.id;
    await user.click(within(row(table, "Claude")).getByRole("button", { name: "Edit Claude" }));

    const dialog = await screen.findByRole("form", { name: "What Claude opens" });
    expect(within(dialog).getByRole("checkbox", { name: "Acme" })).toBeChecked();
    expect(within(dialog).getByRole("checkbox", { name: "Globex" })).toBeChecked();
    // Untick Globex, read only in Acme, under other labels.
    await user.click(within(dialog).getByRole("checkbox", { name: "Globex" }));
    await user.selectOptions(
      within(dialog).getByRole("combobox", { name: "Access in Acme" }),
      "read",
    );
    const labels = within(dialog).getByRole("textbox", { name: "Labels in Acme" });
    await user.clear(labels);
    await user.type(labels, "via:claude team:support");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(screen.queryByRole("form", { name: "What Claude opens" })).toBeNull(),
    );
    const [patch] = callsTo(`/v1/connections/${id}`, "PATCH");
    expect(JSON.parse((await patch?.body) ?? "")).toEqual({
      organizations: [
        { id: "org_01ACME", access: "read", allowedLabels: { via: "claude", team: "support" } },
      ],
    });
    const claude = row(await screen.findByRole("table"), "Claude");
    await waitFor(() => expect(within(claude).queryByText("Globex")).toBeNull());
    expect(within(claude).getByText("Read")).toBeInTheDocument();
  });

  it("refuses to save a connection that opens nothing", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    renderApp("/account/connections", { world });
    const table = await screen.findByRole("table");
    await user.click(
      within(row(table, "Claude Code")).getByRole("button", { name: "Edit Claude Code" }),
    );
    const dialog = await screen.findByRole("form", { name: "What Claude Code opens" });
    await user.click(within(dialog).getByRole("checkbox", { name: "Acme" }));
    expect(within(dialog).getByRole("button", { name: "Save" })).toBeDisabled();
    expect(within(dialog).getByText(/or revoke the connection/i)).toBeInTheDocument();
  });

  it("revokes a connection after asking once more, in a dialog", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { callsTo } = renderApp("/account/connections", { world });
    const table = await screen.findByRole("table");
    const code = row(table, "Claude Code");
    await user.click(within(code).getByRole("button", { name: "Revoke" }));
    const dialog = await screen.findByRole("dialog", { name: "Revoke Claude Code?" });
    // Cancelling revokes nothing.
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const id = world.connections.connections.find((c) => c.client.name === "Claude Code")?.id;
    expect(callsTo(`/v1/connections/${id}`, "DELETE")).toHaveLength(0);

    await user.click(within(code).getByRole("button", { name: "Revoke" }));
    const again = await screen.findByRole("dialog", { name: "Revoke Claude Code?" });
    const confirm = within(again).getByRole("button", { name: "Revoke" });
    expect(confirm).toHaveClass("danger");
    await user.click(confirm);

    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(callsTo(`/v1/connections/${id}`, "DELETE")).toHaveLength(1);
    await waitFor(() => expect(screen.getAllByRole("row")).toHaveLength(2));
    expect(callsTo("/v1/connections", "GET").length).toBeGreaterThanOrEqual(2);
    const revoked = world.connections.connections.find((c) => c.client.name === "Claude Code");
    expect(revoked?.revokedAt).not.toBeNull();
    expect(world.access.grants.get("org_01ACME")?.has(`connection:${revoked?.id}`)).toBe(false);
  });
});
