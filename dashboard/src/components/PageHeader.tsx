// The head of a page, as on Vercel's dashboard: 56 px, the page's name (or where it is) in the
// middle, its actions on the right, on its left what the frame puts there (the namespace picker)
// unless the page has its own. What the page is opens from an (i) beside its name, never as a
// paragraph under it.
import { createContext, type ReactNode, useContext } from "react";
import { InfoCard } from "./InfoCard";

/** What the frame shows at the left of every page's header: the namespace picker. */
export const PageHeaderStart = createContext<ReactNode>(null);

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
  const framed = useContext(PageHeaderStart);
  return (
    <header className="page-header">
      <div className="page-header-start">{start ?? framed}</div>
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
