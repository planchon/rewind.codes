// The head of a page, as on Vercel's dashboard: 56 px, the page's name (or where it is) in the
// middle, its actions on the right, something to the left when the page has it. What the page is
// opens from an (i) beside its name, never as a paragraph under it.
import type { ReactNode } from "react";
import { InfoCard } from "./InfoCard";

export function PageHeader({
  title,
  crumbs,
  meta,
  info,
  start,
  actions,
}: {
  /** The page's name: its heading. */
  title: ReactNode;
  /** What leads to the page, before its name ("Runs / a run"). */
  crumbs?: ReactNode;
  /** What follows its name: a status, tags. */
  meta?: ReactNode;
  /** What the page is, in a card opened from an (i) beside its name. */
  info?: ReactNode;
  start?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <header className="page-header">
      <div className="page-header-start">{start}</div>
      <div className="page-header-title">
        {crumbs && (
          <>
            <span className="page-header-crumbs">{crumbs}</span>
            <span className="page-header-sep" aria-hidden="true">
              /
            </span>
          </>
        )}
        <h1>{title}</h1>
        {meta}
        {info && (
          <InfoCard label={typeof title === "string" ? `About ${title}` : "About this page"}>
            {info}
          </InfoCard>
        )}
      </div>
      <div className="page-header-actions">{actions}</div>
    </header>
  );
}
