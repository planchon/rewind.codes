import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { SidebarResizer } from "./SidebarResizer";
import { SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_RAIL, SidebarProvider } from "./sidebar";

const width = () => document.documentElement.style.getPropertyValue("--sidebar-width");

function Frame() {
  return (
    <SidebarProvider>
      <SidebarResizer />
    </SidebarProvider>
  );
}

afterEach(() => {
  document.documentElement.style.removeProperty("--sidebar-width");
  delete document.documentElement.dataset.sidebar;
});

describe("the sidebar's size", () => {
  it("resizes the sidebar with the keyboard, within its bounds, and keeps the width", () => {
    const { unmount } = render(<Frame />);
    const edge = screen.getByRole("separator", { name: "Resize the sidebar" });
    expect(edge).toHaveAttribute("aria-valuenow", String(SIDEBAR_DEFAULT));
    expect(width()).toBe("");

    fireEvent.keyDown(edge, { key: "ArrowRight" });
    expect(edge).toHaveAttribute("aria-valuenow", String(SIDEBAR_DEFAULT + 16));
    expect(width()).toBe(`${SIDEBAR_DEFAULT + 16}px`);

    fireEvent.keyDown(edge, { key: "End" });
    expect(edge).toHaveAttribute("aria-valuenow", String(SIDEBAR_MAX));
    fireEvent.keyDown(edge, { key: "ArrowRight" });
    expect(edge).toHaveAttribute("aria-valuenow", String(SIDEBAR_MAX));

    // A new page starts at the width kept.
    unmount();
    render(<Frame />);
    expect(screen.getByRole("separator")).toHaveAttribute("aria-valuenow", String(SIDEBAR_MAX));
  });

  it("resizes the sidebar by dragging its edge, without folding it", () => {
    render(<Frame />);
    const edge = screen.getByRole("separator");
    fireEvent.pointerDown(edge, { button: 0, clientX: 256, pointerId: 1 });
    fireEvent.pointerMove(edge, { clientX: 300, pointerId: 1 });
    fireEvent.pointerUp(edge, { clientX: 300, pointerId: 1 });
    expect(edge).toHaveAttribute("aria-valuenow", String(SIDEBAR_DEFAULT + 44));
    expect(width()).toBe(`${SIDEBAR_DEFAULT + 44}px`);
    expect(document.documentElement.dataset.sidebar).toBeUndefined();
  });

  it("folds to a rail of icons and back, and keeps it folded", () => {
    const { unmount } = render(<Frame />);
    // A click on its edge, without moving, folds it.
    const edge = screen.getByRole("separator", { name: "Resize the sidebar" });
    fireEvent.pointerDown(edge, { button: 0, clientX: 256, pointerId: 1 });
    fireEvent.pointerUp(edge, { clientX: 257, pointerId: 1 });
    expect(document.documentElement.dataset.sidebar).toBe("folded");
    expect(width()).toBe(`${SIDEBAR_RAIL}px`);
    // Folded, its edge is no longer dragged: a click on it unfolds it.
    expect(screen.queryByRole("separator")).toBeNull();

    unmount();
    render(<Frame />);
    expect(document.documentElement.dataset.sidebar).toBe("folded");
    fireEvent.click(screen.getByRole("button", { name: "Expand the sidebar" }));
    expect(document.documentElement.dataset.sidebar).toBeUndefined();
    expect(width()).toBe("");
    expect(screen.getByRole("separator")).toBeInTheDocument();
  });
});
