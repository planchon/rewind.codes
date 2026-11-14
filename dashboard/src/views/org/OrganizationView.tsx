// Organization: the members and their grants, the invitations, the roles and their permissions,
// the API keys, the settings; each part shown to whoever holds its permission.
import type { ReactNode } from "react";
import { Outlet } from "react-router";
import { SECTIONS, useAccess } from "../../app/access";
import { NoPermission } from "../../app/NoPermission";
import { Navigate, NavLink } from "../../app/namespace";
import { CopyButton } from "../../components/bits";

export function OrganizationView() {
  const access = useAccess();
  const org = access.organization;
  return (
    <div className="page">
      <div className="page-head">
        <h1>{org ? org.name : "Access"}</h1>
        {org ? (
          <span className="copyable muted">
            <span className="mono">{org.id}</span>
            <CopyButton text={org.id} label="Copy the organization's id" />
          </span>
        ) : (
          access.identity.tenant && (
            <span className="muted">
              tenant <span className="mono">{access.identity.tenant}</span>
            </span>
          )
        )}
      </div>
      {access.sections.length > 0 && (
        <nav className="subnav" aria-label="Organization">
          {access.sections.map((s) => (
            <NavLink key={s.path} to={`/organization/${s.path}`}>
              {s.title}
            </NavLink>
          ))}
        </nav>
      )}
      <Outlet />
    </div>
  );
}

/** The first part the caller may open. */
export function OrganizationIndex() {
  const access = useAccess();
  const first = access.sections[0];
  if (first) return <Navigate to={`/organization/${first.path}`} replace />;
  return <NoPermission permission="members:read" what="see the organization" />;
}

/** A part of the view, shown only to whoever may open it. */
export function SectionGate({ path, children }: { path: string; children: ReactNode }) {
  const access = useAccess();
  const section = SECTIONS.find((s) => s.path === path);
  if (!section) return null;
  if (access.sections.some((s) => s.path === path)) return <>{children}</>;
  if (section.needsOrganization && !access.organization) {
    return (
      <div className="no-permission" role="alert">
        <h1>{section.title}: a member's view</h1>
        <p>
          Members, invitations and settings are those of an organization, seen by its members once
          signed in. An API key reaches its tenant's roles and keys.
        </p>
      </div>
    );
  }
  return (
    <NoPermission permission={section.permission} what={`open ${section.title.toLowerCase()}`} />
  );
}
