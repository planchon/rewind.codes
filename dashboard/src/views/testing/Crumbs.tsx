// The breadcrumb of Testing's pages: Testing / a workflow / a version / a test. The version and
// the test are switchers: a click lists the workflow's versions, or the tests of the version's
// suite, and picks another.
import { Menu } from "@base-ui/react/menu";
import { Check, ChevronsUpDown } from "lucide-react";
import { Fragment, type ReactNode } from "react";
import { Link, useNavigate } from "react-router";

export interface SwitchItem {
  key: string;
  label: ReactNode;
  /** Said at the end of the line: "latest", a count. */
  end?: ReactNode;
  to: string;
  current: boolean;
}

export interface SwitchGroup {
  label: string;
  items: SwitchItem[];
}

/** A segment that lists its siblings and goes to the one picked. */
export function Switcher({
  label,
  title,
  groups,
}: {
  label: ReactNode;
  /** What it switches, for its tooltip: "Version", "Test". */
  title: string;
  groups: SwitchGroup[];
}) {
  const navigate = useNavigate();
  return (
    <Menu.Root>
      <Menu.Trigger className="crumb-switch" title={`${title}: pick another`}>
        {label}
        <ChevronsUpDown size={14} aria-hidden="true" className="crumb-switch-icon" />
      </Menu.Trigger>
      <Menu.Portal>
        <Menu.Positioner className="menu-positioner" side="bottom" align="start" sideOffset={6}>
          <Menu.Popup className="menu-popup crumb-menu" aria-label={title}>
            {groups
              .filter((group) => group.items.length > 0)
              .map((group, i) => (
                <Fragment key={group.label}>
                  {i > 0 && <Menu.Separator className="menu-separator" />}
                  <Menu.Group>
                    <Menu.GroupLabel className="menu-label">{group.label}</Menu.GroupLabel>
                    {group.items.map((item) => (
                      <Menu.Item
                        key={item.key}
                        className="menu-item"
                        aria-current={item.current ? "page" : undefined}
                        onClick={() => void navigate(item.to)}
                      >
                        <span className="menu-item-icon">
                          {item.current && <Check size={14} aria-hidden="true" />}
                        </span>
                        <span className="menu-item-text">{item.label}</span>
                        {item.end && <span className="menu-item-end">{item.end}</span>}
                      </Menu.Item>
                    ))}
                  </Menu.Group>
                </Fragment>
              ))}
          </Menu.Popup>
        </Menu.Positioner>
      </Menu.Portal>
    </Menu.Root>
  );
}

/** The segments before the page's own, each a link or a switcher, with their separators. */
export function Crumbs({ children }: { children: ReactNode[] }) {
  return (
    <span className="crumbs">
      {children.map((child, i) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: the segments are in a fixed order.
        <Fragment key={i}>
          {i > 0 && (
            <span className="page-header-sep" aria-hidden="true">
              /
            </span>
          )}
          {child}
        </Fragment>
      ))}
    </span>
  );
}

export function CrumbLink({ to, children }: { to: string; children: ReactNode }) {
  return <Link to={to}>{children}</Link>;
}
