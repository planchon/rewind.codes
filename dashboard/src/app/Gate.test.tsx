import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { createWorld } from "../mocks/fixtures";
import { NOW, renderApp } from "../test/app";

describe("Gate", () => {
  it("lets the operator in at once when the server needs no key", async () => {
    renderApp("/runs");
    expect(await screen.findByRole("heading", { name: "Runs" })).toBeInTheDocument();
    expect(screen.getByText(/ptc 0\.1\.0/)).toBeInTheDocument();
  });

  it("asks for a key, says when it is refused, and keeps it for the tab only", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW, requireKey: "ptc_sk_good" });
    const { requested } = renderApp("/runs", { world });

    const input = await screen.findByLabelText("API key");
    await user.type(input, "ptc_sk_wrong");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      /refused this key.*unknown or revoked/i,
    );

    await user.clear(screen.getByLabelText("API key"));
    await user.type(screen.getByLabelText("API key"), "ptc_sk_good");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect(await screen.findByRole("heading", { name: "Runs" })).toBeInTheDocument();
    expect(screen.getByText("ptc_sk_…good")).toBeInTheDocument();
    expect(window.sessionStorage.getItem("ptc.key")).toBe("ptc_sk_good");
    expect(window.localStorage.getItem("ptc.key")).toBeNull();
    await waitFor(() => expect(requested("/v1/runs").length).toBeGreaterThan(0));
  });

  it("remembers the key on this device when asked, and forgets it on sign out", async () => {
    const user = userEvent.setup();
    const world = createWorld({ now: NOW, requireKey: "ptc_sk_good" });
    renderApp("/runs", { world });
    await user.type(await screen.findByLabelText("API key"), "ptc_sk_good");
    await user.click(screen.getByLabelText("Remember on this device"));
    await user.click(screen.getByRole("button", { name: /continue/i }));
    await screen.findByRole("heading", { name: "Runs" });
    expect(window.localStorage.getItem("ptc.key")).toBe("ptc_sk_good");

    await user.click(screen.getByRole("button", { name: "Sign out" }));
    expect(await screen.findByLabelText("API key")).toBeInTheDocument();
    expect(window.localStorage.getItem("ptc.key")).toBeNull();
    expect(window.sessionStorage.getItem("ptc.key")).toBeNull();
  });
});
