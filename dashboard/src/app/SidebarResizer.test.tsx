import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, SidebarResizer } from "./SidebarResizer";

const width = () => document.documentElement.style.getPropertyValue("--sidebar-width");

afterEach(() => document.documentElement.style.removeProperty("--sidebar-width"));

describe("the sidebar's edge", () => {
  it("resizes the sidebar with the keyboard, within its bounds, and keeps the width", () => {
    const { unmount } = render(<SidebarResizer />);
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
    render(<SidebarResizer />);
    expect(screen.getByRole("separator")).toHaveAttribute("aria-valuenow", String(SIDEBAR_MAX));
  });

  it("gives the default width back on a double-click", () => {
    render(<SidebarResizer />);
    const edge = screen.getByRole("separator");
    fireEvent.keyDown(edge, { key: "Home" });
    expect(edge).toHaveAttribute("aria-valuenow", String(SIDEBAR_MIN));
    fireEvent.doubleClick(edge);
    expect(edge).toHaveAttribute("aria-valuenow", String(SIDEBAR_DEFAULT));
    expect(width()).toBe("");
  });
});
