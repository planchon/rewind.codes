import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { adoptHashRoute, keepRoute, restoreRoute } from "../../app/pendingRoute";
import { CLAUDE_CALLBACK, CLAUDE_CODE, consentRequest } from "../../mocks/connections";
import { createWorld, type World } from "../../mocks/fixtures";
import { NOW, renderApp, signedIn } from "../../test/app";

/** Ada in Acme, with a request of claude.ai waiting for her consent. */
function adaWithRequest(options: Parameters<typeof consentRequest>[1] = {}) {
  const world = signedIn("ada@example.net", "org_01ACME");
  const request = consentRequest(world.connections, options);
  return { world, request };
}

function orgRow(name: string): HTMLElement {
  const row = screen
    .getByRole("checkbox", { name: new RegExp(`^${name}`) })
    .closest(".consent-org");
  if (!(row instanceof HTMLElement)) throw new Error(`no row for ${name}`);
  return row;
}

describe("the consent", () => {
  it("ticks the organization open in the dashboard, and offers only the accesses the person holds", async () => {
    const { world, request } = adaWithRequest();
    renderApp(`/connect/authorize?request=${request.id}`, { world });
    expect(
      await screen.findByRole("heading", { name: "Claude wants to access Rewind" }),
    ).toBeInTheDocument();
    expect(screen.getByText("claude.ai", { selector: "strong" })).toBeInTheDocument();
    expect(screen.getByText(/signed in as/i)).toHaveTextContent("Ada Lovelace");

    // Acme, where she is owner, is ticked with Run; Globex, where she reads, offers Read only.
    expect(screen.getByRole("checkbox", { name: /^Acme/ })).toBeChecked();
    expect(screen.getByRole("combobox", { name: "Access in Acme" })).toHaveValue("run");
    expect(screen.getByRole("checkbox", { name: /^Globex/ })).not.toBeChecked();
    const globex = screen.getByRole("combobox", { name: "Access in Globex" });
    expect(within(globex).getByRole("option", { name: "Run" })).toBeDisabled();
    expect(within(globex).getByRole("option", { name: "Read" })).toBeEnabled();
    expect(within(orgRow("Globex")).getByText("reader")).toBeInTheDocument();

    expect(
      screen.getByRole("checkbox", { name: "Only what Claude starts (via=claude)" }),
    ).toBeChecked();
    expect(screen.queryByRole("note")).toBeNull();
  });

  it("cannot tick an organization where the person may give nothing, nor allow with none", async () => {
    const user = userEvent.setup();
    const { world, request } = adaWithRequest();
    // In Globex, Ada now only works harness actions' calls: neither access.
    world.access.grants
      .get("org_01GLOBEX")
      ?.set("member:user_01ADA", { role: "worker", allowedLabels: {} });
    renderApp(`/connect/authorize?request=${request.id}`, { world });
    const globex = await screen.findByRole("checkbox", { name: /^Globex/ });
    expect(globex).toBeDisabled();
    expect(within(orgRow("Globex")).getByText(/gives neither access/i)).toBeInTheDocument();
    expect(screen.queryByRole("combobox", { name: "Access in Globex" })).toBeNull();

    await user.click(screen.getByRole("checkbox", { name: /^Acme/ }));
    expect(screen.getByRole("button", { name: "Allow" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Deny" })).toBeEnabled();
  });

  it("allows: posts the organizations, accesses and labels chosen, then returns to Claude", async () => {
    const user = userEvent.setup();
    const assign = vi.fn();
    const { world, request } = adaWithRequest();
    const { callsTo } = renderApp(`/connect/authorize?request=${request.id}`, { world, assign });
    await screen.findByRole("heading", { name: "Claude wants to access Rewind" });
    await user.selectOptions(screen.getByRole("combobox", { name: "Access in Acme" }), "read");
    await user.click(screen.getByRole("checkbox", { name: /^Globex/ }));
    await user.click(screen.getByRole("button", { name: "Allow" }));

    await waitFor(() => expect(assign).toHaveBeenCalledTimes(1));
    const [post] = callsTo("/v1/oauth/consent", "POST");
    expect(JSON.parse((await post?.body) ?? "")).toEqual({
      request: request.id,
      decision: "allow",
      organizations: [
        { id: "org_01ACME", access: "read" },
        { id: "org_01GLOBEX", access: "read" },
      ],
      allowedLabels: { via: "claude" },
    });
    const back = new URL(String(assign.mock.calls[0]?.[0]));
    expect(`${back.origin}${back.pathname}`).toBe(CLAUDE_CALLBACK);
    expect(back.searchParams.get("state")).toBe(request.state);
    expect(back.searchParams.get("code")).toBeTruthy();
    expect(await screen.findByRole("heading", { name: /returning to claude/i })).toBeVisible();

    // The connection exists, with its grants.
    const made = world.connections.connections.at(-1);
    expect(made?.userId).toBe("user_01ADA");
    expect(world.access.grants.get("org_01GLOBEX")?.get(`connection:${made?.id}`)?.role).toBe(
      "reader",
    );
  });

  it("sends no labels when the person unticks them", async () => {
    const user = userEvent.setup();
    const { world, request } = adaWithRequest();
    const { callsTo } = renderApp(`/connect/authorize?request=${request.id}`, {
      world,
      assign: vi.fn(),
    });
    await user.click(
      await screen.findByRole("checkbox", { name: "Only what Claude starts (via=claude)" }),
    );
    await user.click(screen.getByRole("button", { name: "Allow" }));
    await waitFor(() => expect(callsTo("/v1/oauth/consent", "POST")).toHaveLength(1));
    const [post] = callsTo("/v1/oauth/consent", "POST");
    expect(JSON.parse((await post?.body) ?? "")).toMatchObject({
      organizations: [{ id: "org_01ACME", access: "run" }],
      allowedLabels: {},
    });
  });

  it("denies: returns to Claude with access_denied, and makes no connection", async () => {
    const user = userEvent.setup();
    const assign = vi.fn();
    const { world, request } = adaWithRequest();
    const before = world.connections.connections.length;
    const { callsTo } = renderApp(`/connect/authorize?request=${request.id}`, { world, assign });
    await user.click(await screen.findByRole("button", { name: "Deny" }));
    await waitFor(() => expect(assign).toHaveBeenCalledTimes(1));
    const [post] = callsTo("/v1/oauth/consent", "POST");
    expect(JSON.parse((await post?.body) ?? "")).toMatchObject({
      request: request.id,
      decision: "deny",
      organizations: [],
    });
    const back = new URL(String(assign.mock.calls[0]?.[0]));
    expect(back.searchParams.get("error")).toBe("access_denied");
    expect(back.searchParams.get("state")).toBe(request.state);
    expect(world.connections.connections).toHaveLength(before);
  });

  it("warns of a loopback return and of a client that registered itself", async () => {
    const { world, request } = adaWithRequest({
      client: { ...CLAUDE_CODE, kind: "registered", logoUri: "https://evil.example/logo.png" },
      redirectUri: "http://localhost:54321/callback",
    });
    renderApp(`/connect/authorize?request=${request.id}`, { world });
    await screen.findByRole("heading", { name: "Claude Code wants to access Rewind" });
    const notes = screen.getAllByRole("note");
    expect(notes[0]).toHaveTextContent(/a program on this computer \(localhost:54321\)/i);
    expect(notes[1]).toHaveTextContent(/registered itself/i);
    // A client's logo is never loaded: an image from its server.
    expect(document.querySelector('img[src^="https://evil.example"]')).toBeNull();
  });

  it("says plainly that an expired or unknown request must start again from Claude", async () => {
    const { world, request } = adaWithRequest({ expiresInMs: -1_000 });
    const { unmount } = renderApp(`/connect/authorize?request=${request.id}`, { world });
    expect(await screen.findByRole("heading", { name: "This request has expired" })).toBeVisible();
    expect(screen.getByText(/go back to claude/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Allow" })).toBeNull();
    unmount();

    renderApp("/connect/authorize?request=nothing", { world });
    expect(await screen.findByRole("heading", { name: "This request has expired" })).toBeVisible();
  });

  it("signs in first, then comes back to the consent", async () => {
    const user = userEvent.setup();
    const world: World = createWorld({ now: NOW, workos: true });
    const request = consentRequest(world.connections);
    renderApp(`/connect/authorize?request=${request.id}`, { world });
    await user.type(await screen.findByLabelText("Email"), "grace@example.net");
    await user.click(screen.getByRole("button", { name: /send me a code/i }));
    await screen.findByRole("heading", { name: "Check your email" });
    await user.type(
      screen.getByLabelText("Code"),
      world.access.codes.get("grace@example.net") ?? "",
    );
    expect(
      await screen.findByRole("heading", { name: "Claude wants to access Rewind" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: /^Acme/ })).toBeChecked();
  });

  it("keeps the consent's address through GitHub's round trip", async () => {
    const user = userEvent.setup();
    const assign = vi.fn();
    const world = createWorld({ now: NOW, workos: true });
    const address = "/connect/authorize?request=abc123";
    window.history.replaceState(null, "", address);
    renderApp(address, { world, assign });
    await user.click(await screen.findByRole("button", { name: /continue with github/i }));
    expect(assign).toHaveBeenCalledWith(`${window.location.origin}/auth/github`);

    // GitHub's callback lands on `/`: the app puts the consent back before routing.
    window.history.replaceState(null, "", "/");
    restoreRoute();
    expect(`${window.location.pathname}${window.location.search}`).toBe(address);
    // Once only.
    window.history.replaceState(null, "", "/");
    restoreRoute();
    expect(window.location.pathname).toBe("/");
  });

  it("keeps what the callback says when it puts the route back", () => {
    window.history.replaceState(null, "", "/connect/authorize?request=abc");
    keepRoute();
    window.history.replaceState(null, "", "/?auth_error=state_mismatch");
    restoreRoute();
    expect(`${window.location.pathname}${window.location.search}`).toBe(
      "/connect/authorize?request=abc&auth_error=state_mismatch",
    );
  });

  it("forgets a route kept too long ago, and never replaces a deep link", () => {
    window.history.replaceState(null, "", "/connect/authorize?request=old");
    keepRoute();
    window.history.replaceState(null, "", "/");
    restoreRoute(Date.now() + 16 * 60_000);
    expect(window.location.pathname).toBe("/");

    window.history.replaceState(null, "", "/connect/authorize?request=x");
    keepRoute();
    window.history.replaceState(null, "", "/runs/abc");
    restoreRoute();
    expect(window.location.pathname).toBe("/runs/abc");
  });

  it("reads a link of an earlier version, its route in the fragment, as the path it meant", () => {
    window.history.replaceState(null, "", "/#/runs/abc?step=3");
    adoptHashRoute();
    expect(`${window.location.pathname}${window.location.search}${window.location.hash}`).toBe(
      "/runs/abc?step=3",
    );
    window.history.replaceState(null, "", "/?auth_error=provider_error#/");
    adoptHashRoute();
    expect(`${window.location.pathname}${window.location.search}${window.location.hash}`).toBe(
      "/?auth_error=provider_error",
    );
  });

  it("sends a member without organization through the onboarding first, telling them why", async () => {
    window.history.replaceState(null, "", "/connect/authorize?request=abc");
    const world = signedIn("margaret@example.net");
    renderApp("/connect/authorize?request=abc", { world });
    expect(await screen.findByRole("note")).toHaveTextContent(/claude is waiting for your answer/i);
  });
});
