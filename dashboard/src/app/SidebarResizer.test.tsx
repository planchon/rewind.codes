import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { SidebarFold, SidebarResizer } from "./SidebarResizer";
import {
  SIDEBAR_DEFAULT,
  SIDEBAR_MAX,
  SIDEBAR_MIN,
  SIDEBAR_RAIL,
  SidebarProvider,
} from "./sidebar";

const width = () => document.documentElement.style.getPropertyValue("--sidebar-width");

function Frame() {
  return (
    <SidebarProvider>
      <SidebarFold />
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

  it("gives the default width back on a double-click", () => {
    render(<Frame />);
    const edge = screen.getByRole("separator");
    fireEvent.keyDown(edge, { key: "Home" });
    expect(edge).toHaveAttribute("aria-valuenow", String(SIDEBAR_MIN));
    fireEvent.doubleClick(edge);
    expect(edge).toHaveAttribute("aria-valuenow", String(SIDEBAR_DEFAULT));
    expect(width()).toBe("");
  });

  it("folds to a rail of icons and back, and keeps it folded", () => {
    const { unmount } = render(<Frame />);
    fireEvent.click(screen.getByRole("button", { name: /Collapse the sidebar/ }));
    expect(document.documentElement.dataset.sidebar).toBe("folded");
    expect(width()).toBe(`${SIDEBAR_RAIL}px`);
    // Folded, it has no edge to drag.
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
