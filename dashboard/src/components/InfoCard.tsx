// An (i) beside a title: what the page or the section is, in a card that opens on hover, on focus
// and on a click, rather than a paragraph that takes the page's room.
import { Info } from "lucide-react";
import type { ReactNode } from "react";
import { HoverCard } from "./HoverCard";

export function InfoCard({ label, children }: { label: string; children: ReactNode }) {
  return (
    <HoverCard
      label={label}
      className="info-card"
      trigger={
        <button type="button" className="info-btn" aria-label={label}>
          <Info size={14} aria-hidden="true" />
        </button>
      }
    >
      {children}
    </HoverCard>
  );
}
