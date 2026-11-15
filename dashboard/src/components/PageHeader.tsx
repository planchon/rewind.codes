// The head of a page, as on Vercel's dashboard: 56 px, the page's name (or where it is) in the
// middle, its actions on the right, something to the left when the page has it.
import type { ReactNode } from "react";

export function PageHeader({
  title,
  crumbs,
  start,
  actions,
}: {
  /** The page's name: its heading. */
  title: ReactNode;
  /** What leads to the page, before its name ("Observability / a run"). */
  crumbs?: ReactNode;
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
      </div>
      <div className="page-header-actions">{actions}</div>
    </header>
  );
}
