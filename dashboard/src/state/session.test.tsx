import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { World } from "../mocks/fixtures";
import { NOW, renderApp, signedIn } from "../test/app";
import { REFRESH_MARGIN_MS, refreshDelay } from "./session";

function currentToken(world: World): string {
  const session = world.access.session;
  if (session?.step !== "authenticated") throw new Error("not signed in");
  return session.token;
}

describe("the session", () => {
  it("asks for a fresh token a minute before it expires", () => {
    const now = Date.parse("2026-11-14T10:00:00Z");
    const expiresAt = new Date(now + 5 * 60_000).toISOString();
    expect(refreshDelay(expiresAt, 0, false, now)).toBe(4 * 60_000);
    // The server's clock is 30 s ahead: ask 30 s sooner.
    expect(refreshDelay(expiresAt, 30_000, false, now)).toBe(4 * 60_000 - 30_000);
    // The server kept the same token: wait before asking again.
    expect(refreshDelay(new Date(now + REFRESH_MARGIN_MS).toISOString(), 0, true, now)).toBe(
      10_000,
    );
    // Already late: ask at once, but never in a loop.
    expect(refreshDelay(new Date(now).toISOString(), 0, false, now)).toBe(1_000);
  });

  it("refreshes the access token before it expires, and sends the new one", async () => {
    // Tokens that live a minute and a second: the dashboard asks again after a second.
    const world = signedIn("grace@example.net", undefined, { now: NOW, tokenTtlMs: 61_000 });
    const first = currentToken(world);
    const { callsTo, router } = renderApp("/runs", { world });
    expect(await screen.findByRole("heading", { name: "All" })).toBeInTheDocument();
    await waitFor(() =>
      expect(callsTo("/v1/runs")[0]?.headers.get("Authorization")).toBe(`Bearer ${first}`),
    );

    await waitFor(() => expect(currentToken(world)).not.toBe(first), { timeout: 4_000 });
    const second = currentToken(world);
    await router.navigate("/workflows");
    await waitFor(() =>
      expect(callsTo("/v1/workflows").at(-1)?.headers.get("Authorization")).toBe(
        `Bearer ${second}`,
      ),
    );
  });

  it("renews a refused token, and sends the request again with the new one", async () => {
    const world = signedIn("grace@example.net");
    const { callsTo, router } = renderApp("/runs", { world });
    await screen.findByRole("heading", { name: "All" });
    const before = callsTo("/auth/session").length;
    const already = callsTo("/v1/workflows").length;
    const first = currentToken(world);

    // The token is refused (expired, say), but the session holds.
    world.access.tokens.clear();
    await router.navigate("/workflows");
    expect(await screen.findByRole("heading", { name: "Workflows" })).toBeInTheDocument();
    expect(await screen.findByRole("table")).toBeInTheDocument();
    // Whichever request of the page met the refusal first (the workflows, their counts, the change
    // stream) renewed the token; every refused one went again with the new one.
    expect(callsTo("/auth/session").length).toBeGreaterThan(before);
    const workflows = callsTo("/v1/workflows").slice(already);
    expect(workflows.at(-1)?.headers.get("Authorization")).toBe(`Bearer ${currentToken(world)}`);
    expect(currentToken(world)).not.toBe(first);
    const refused = workflows.filter((c) => c.headers.get("Authorization") === `Bearer ${first}`);
    expect(refused.length).toBeLessThanOrEqual(1);
  });

  it("goes back to the sign-in page when the session has ended", async () => {
    const world = signedIn("grace@example.net");
    const { router } = renderApp("/runs", { world });
    await screen.findByRole("heading", { name: "All" });

    world.access.tokens.clear();
    world.access.session = null;
    await router.navigate("/workflows");
    expect(await screen.findByRole("heading", { name: "Sign in to Rewind" })).toBeVisible();
    expect(screen.getByRole("status")).toHaveTextContent(/session has ended/i);
  });

  it("signs out", async () => {
    const user = userEvent.setup();
    const world = signedIn("grace@example.net");
    const { callsTo } = renderApp("/runs", { world });
    await screen.findByRole("heading", { name: "All" });
    await user.click(screen.getByLabelText("Account: Grace Hopper"));
    expect(await screen.findByText("grace@example.net")).toBeVisible();
    await user.click(screen.getByRole("menuitem", { name: "Sign out" }));
    expect(await screen.findByRole("heading", { name: "Sign in to Rewind" })).toBeVisible();
    const [out] = callsTo("/auth/logout", "POST");
    expect(out?.headers.get("Content-Type")).toMatch(/application\/json/);
    expect(world.access.session).toBeNull();
  });

  it("switches organization, and shows nothing read in the previous one", async () => {
    const user = userEvent.setup();
    const world = signedIn("ada@example.net", "org_01ACME");
    const { callsTo, router } = renderApp("/runs", { world });
    expect((await screen.findAllByText("c-42")).length).toBeGreaterThan(0);

    await user.click(screen.getByLabelText("Organization: Acme"));
    const list = await screen.findByRole("group", { name: "Organizations" });
    await user.click(within(list).getByRole("menuitem", { name: "Globex" }));

    expect(await screen.findByLabelText("Organization: Globex")).toBeInTheDocument();
    const switched = callsTo("/auth/session", "POST").at(-1);
    expect(JSON.parse((await switched?.body) ?? "")).toEqual({ organizationId: "org_01GLOBEX" });
    // The new organization opens on its home page, with none of Acme's runs.
    await waitFor(() => expect(router.state.location.pathname).toBe("/overview"));
    expect(await screen.findByRole("heading", { name: "Overview" })).toBeInTheDocument();
    expect(screen.queryByText("c-42")).toBeNull();
  });
});
