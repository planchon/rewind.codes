import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { renderApp, signedIn } from "../test/app";

describe("the onboarding", () => {
  it("lets a member without organization join the one they were invited to", async () => {
    const user = userEvent.setup();
    // Margaret was approved and invited to Globex, and has no organization yet.
    const world = signedIn("margaret@example.net");
    const { callsTo } = renderApp("/", { world });
    expect(
      await screen.findByRole("heading", { name: "Welcome, Margaret Hamilton" }),
    ).toBeInTheDocument();
    const invitations = screen.getByRole("region", { name: "Invitations" });
    expect(await within(invitations).findByText("harness")).toBeInTheDocument();

    await user.click(within(invitations).getByRole("button", { name: "Join Globex" }));
    expect(await screen.findByLabelText("Organization: Globex")).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "Overview" })).toBeInTheDocument();
    const [accepted] = callsTo(
      `/v1/invitations/${world.access.invitations.find((i) => i.email === "margaret@example.net")?.id}/accept`,
      "POST",
    );
    expect(accepted).toBeDefined();
    const switched = callsTo("/auth/session", "POST").at(-1);
    expect(JSON.parse((await switched?.body) ?? "")).toEqual({ organizationId: "org_01GLOBEX" });
    // The invitation's grant is hers.
    expect(world.access.grants.get("org_01GLOBEX")?.get("user:user_01MARGARET")?.role).toBe(
      "harness",
    );
  });

  it("creates an organization, whose creator is the owner", async () => {
    const user = userEvent.setup();
    const world = signedIn("margaret@example.net");
    renderApp("/", { world });
    const create = await screen.findByRole("region", { name: "Create an organization" });
    await user.type(within(create).getByLabelText("Name"), "Initech");
    await user.click(within(create).getByRole("button", { name: "Create" }));

    expect(await screen.findByLabelText("Organization: Initech")).toBeInTheDocument();
    // An owner sees the whole Organization view, opened from the account menu.
    await user.click(screen.getByLabelText("Account: Margaret Hamilton"));
    await user.click(await screen.findByRole("menuitem", { name: "Organization" }));
    const nav = await screen.findByRole("navigation", { name: "Organization" });
    expect(
      within(nav)
        .getAllByRole("link")
        .map((a) => a.textContent),
    ).toEqual(["Members", "Invitations", "Roles", "API keys", "Settings"]);
    const table = await screen.findByRole("table");
    await waitFor(() => expect(within(table).getByText("Margaret Hamilton")).toBeInTheDocument());
    expect(within(table).getByText("owner")).toBeInTheDocument();
  });

  it("says when no invitation is waiting", async () => {
    const world = signedIn("margaret@example.net");
    world.access.invitations = [];
    renderApp("/", { world });
    expect(
      await screen.findByText(/no invitation is waiting for margaret@example.net/i),
    ).toBeInTheDocument();
  });
});
