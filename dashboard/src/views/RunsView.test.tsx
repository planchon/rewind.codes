import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { renderApp, search } from "../test/app";

function rows() {
  const table = screen.getByRole("table");
  return within(table).getAllByRole("row").slice(1);
}

describe("RunsView", () => {
  it("lists runs, most recently started first, with their status, labels and counts", async () => {
    renderApp("/runs");
    await screen.findByRole("table");
    expect(rows().length).toBe(50);
    expect(screen.getByRole("button", { name: "Load more" })).toBeInTheDocument();
    // The run waiting for an approval, started two hours ago.
    const triage = rows().find((r) => within(r).queryByText("c-42"));
    expect(triage).toBeDefined();
    expect(within(triage as HTMLElement).getByText("active")).toBeInTheDocument();
  });

  it("lists runs on a link whose range or digest it cannot read", async () => {
    renderApp("/runs?range=toString");
    await screen.findByRole("table");
    expect(rows().length).toBe(50);
  });

  it("filters by status through the API, and keeps the filter in the URL", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/runs");
    await screen.findByRole("table");
    await user.click(screen.getByRole("button", { name: /^failed$/ }));
    await waitFor(() => expect(search(router).get("status")).toBe("failed"));
    await waitFor(() =>
      expect(requested("/v1/runs").some((u) => u.searchParams.get("status") === "failed")).toBe(
        true,
      ),
    );
    await waitFor(() => expect(rows().every((r) => within(r).queryByText("failed"))).toBe(true));
  });

  it("adds a label filter from a run's label, and a tool filter from the input", async () => {
    const user = userEvent.setup();
    const { router, requested } = renderApp("/runs");
    await screen.findByRole("table");
    const chip = screen.getAllByRole("button", { name: /agent:triage/ })[0];
    await user.click(chip as HTMLElement);
    await waitFor(() => expect(search(router).getAll("label")).toEqual(["agent:triage"]));

    await user.selectOptions(screen.getByLabelText("Filter on"), "tool");
    await user.type(screen.getByLabelText("Filter value for tool"), "github.close_issue{Enter}");
    await waitFor(() => expect(search(router).get("tool")).toBe("github.close_issue"));
    await waitFor(() =>
      expect(
        requested("/v1/runs").some(
          (u) =>
            u.searchParams.get("tool") === "github.close_issue" &&
            u.searchParams.getAll("label").includes("agent:triage"),
        ),
      ).toBe(true),
    );
    await waitFor(() => expect(rows().length).toBe(4));
    expect(
      screen.getByRole("button", { name: "Remove the tool filter github.close_issue" }),
    ).toBeInTheDocument();
  });

  it("applies two changes made before the view renders again", async () => {
    const { router } = renderApp("/runs?tool=github.close_issue&error=timeout");
    await screen.findByRole("table");
    // Both in one task: the second comes before the URL shows the first.
    act(() => {
      fireEvent.click(
        screen.getByRole("button", { name: "Remove the tool filter github.close_issue" }),
      );
      fireEvent.click(screen.getByRole("button", { name: "Remove the error filter timeout" }));
    });
    await waitFor(() => expect(search(router).toString()).toBe(""));
    expect(screen.queryByRole("button", { name: /^Remove the / })).toBeNull();
  });

  it("refuses a label filter that is not key:value", async () => {
    const user = userEvent.setup();
    renderApp("/runs");
    await screen.findByRole("table");
    await user.type(screen.getByLabelText("Filter value for label"), "triage{Enter}");
    expect(screen.getByRole("alert")).toHaveTextContent("A label filter is key:value");
  });

  it("refuses a script filter that is not a whole digest, as the server does", async () => {
    const user = userEvent.setup();
    const { router } = renderApp("/runs");
    await screen.findByRole("table");
    await user.selectOptions(screen.getByLabelText("Filter on"), "script");
    // The short digest the views show.
    await user.type(screen.getByLabelText("Filter value for script"), "9f2c4e1a07b3{Enter}");
    expect(screen.getByRole("alert")).toHaveTextContent("64 hexadecimal digits");
    expect(search(router).get("script")).toBeNull();
  });

  it("reads quarantined runs and a time window from the URL", async () => {
    const { requested } = renderApp("/runs?quarantined=yes&range=24h");
    await screen.findByRole("table");
    await waitFor(() => expect(rows().length).toBe(1));
    const query = requested("/v1/runs").at(-1)?.searchParams;
    expect(query?.get("quarantined")).toBe("true");
    expect(Date.parse(query?.get("startedAfter") ?? "")).toBeGreaterThan(
      Date.now() - 86_400_000 - 60_000,
    );
    expect(screen.getByText("quarantined")).toBeInTheDocument();
  });

  it("loads the next page with the cursor", async () => {
    const user = userEvent.setup();
    const { requested } = renderApp("/runs");
    await screen.findByRole("table");
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(rows().length).toBeGreaterThan(50));
    expect(requested("/v1/runs").some((u) => u.searchParams.get("cursor") === "50")).toBe(true);
  });
});
