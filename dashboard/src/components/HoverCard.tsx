// A card that explains what a cell abbreviates: it opens on hover, on focus and on a click or a
// tap (Base UI's popover, `openOnHover`), and stays open while the pointer is over it, so what it
// holds can be clicked (a label to filter by, the run to open).
import { Popover } from "@base-ui/react/popover";
import type { ReactElement, ReactNode } from "react";

export function HoverCard({
  trigger,
  children,
  label,
  onOpenChange,
  className,
}: {
  /** The element that opens it: a `<button>`. */
  trigger: ReactElement;
  children: ReactNode;
  /** What the card is, for assistive technologies. */
  label: string;
  onOpenChange?: (open: boolean) => void;
  className?: string;
}) {
  return (
    <Popover.Root {...(onOpenChange ? { onOpenChange } : {})}>
      <Popover.Trigger openOnHover delay={200} closeDelay={150} render={trigger} />
      <Popover.Portal>
        <Popover.Positioner
          className="hover-card-positioner"
          side="bottom"
          align="start"
          sideOffset={6}
          collisionPadding={12}
        >
          <Popover.Popup
            className={`hover-card${className ? ` ${className}` : ""}`}
            aria-label={label}
            onClick={(e) => e.stopPropagation()}
          >
            {children}
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  );
}
