import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { createWorld } from "../mocks/fixtures";
import { createHandlers } from "../mocks/handlers";
import { NOW, renderApp } from "../test/app";
import { server } from "../test/server";

describe("Gate", () => {
  it("lets the operator in at once when the server needs no key", async () => {
    renderApp("/runs");
    expect(await screen.findByRole("heading", { name: "All" })).toBeInTheDocument();
    await userEvent.setup().click(screen.getByLabelText("Account: API key"));
    expect(await screen.findByText(/rewind 0\.1\.0/)).toBeInTheDocument();
  });

  it("asks for a key, says when it is refused, and keeps it for the tab only", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW, requireKey: "rewind_sk_good" });
    const { requested } = renderApp("/runs", { world });

    const input = await screen.findByLabelText("API key");
    await user.type(input, "rewind_sk_wrong");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      /refused this key.*unknown or revoked/i,
    );

    await user.clear(screen.getByLabelText("API key"));
    await user.type(screen.getByLabelText("API key"), "rewind_sk_good");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect(await screen.findByRole("heading", { name: "All" })).toBeInTheDocument();
    expect(screen.getByText("rewind_sk_…good")).toBeInTheDocument();
    expect(window.sessionStorage.getItem("rewind.key")).toBe("rewind_sk_good");
    expect(window.localStorage.getItem("rewind.key")).toBeNull();
    await waitFor(() => expect(requested("/v1/runs").length).toBeGreaterThan(0));
  });

  it("remembers the key on this device when asked, and forgets it on sign out", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW, requireKey: "rewind_sk_good" });
    renderApp("/runs", { world });
    await user.type(await screen.findByLabelText("API key"), "rewind_sk_good");
    await user.click(screen.getByLabelText("Remember on this device"));
    await user.click(screen.getByRole("button", { name: /continue/i }));
    await screen.findByRole("heading", { name: "All" });
    expect(window.localStorage.getItem("rewind.key")).toBe("rewind_sk_good");

    await user.click(screen.getByLabelText("Account: API key"));
    await user.click(await screen.findByRole("menuitem", { name: "Sign out" }));
    expect(await screen.findByLabelText("API key")).toBeInTheDocument();
    expect(window.localStorage.getItem("rewind.key")).toBeNull();
    expect(window.sessionStorage.getItem("rewind.key")).toBeNull();
  });

  it("shows nothing read with the previous key after signing in with another", async () => {
    const user = userEvent.setup();
    const first = createWorld({ now: NOW, requireKey: "rewind_sk_first" });
    renderApp("/runs", { world: first });
    await user.type(await screen.findByLabelText("API key"), "rewind_sk_first");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect((await screen.findAllByText("c-42")).length).toBeGreaterThan(0);
    await user.click(screen.getByLabelText("Account: API key"));
    await user.click(await screen.findByRole("menuitem", { name: "Sign out" }));
    await screen.findByLabelText("API key");

    // Another tenant's key, whose runs are slow to come.
    const second = createWorld({ now: NOW, requireKey: "rewind_sk_second" });
    let release = () => {};
    const slow = new Promise<void>((resolve) => {
      release = resolve;
    });
    server.use(
      http.get("*/v1/runs", async () => {
        await slow;
        return HttpResponse.json({ runs: [] });
      }),
      ...createHandlers(second, { now: () => NOW, pollMs: 10 }),
    );
    await user.type(screen.getByLabelText("API key"), "rewind_sk_second");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect(await screen.findByRole("heading", { name: "All" })).toBeInTheDocument();
    expect(screen.queryByText("c-42")).toBeNull();
    release();
    expect(await screen.findByText("No run yet.")).toBeInTheDocument();
    expect(screen.queryByText("c-42")).toBeNull();
  });
});
