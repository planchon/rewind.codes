// Organization, opened from the account menu: the members and their grants, the invitations, the
// roles and their permissions, the API keys, the settings; each part shown to whoever holds its
// permission, and listed in the organization's own sidebar.

import { KeyRound, Mail, Plug, Shield, SlidersHorizontal, Users } from "lucide-react";
import type { ReactNode } from "react";
import { Navigate, NavLink, Outlet, useLocation } from "react-router";
import { OrgAvatar } from "../../app/AccountMenus";
import { NoPermission } from "../../app/NoPermission";
import { PageSidebar, useSidebarFolded } from "../../app/sidebar";
import { CopyButton } from "../../components/bits";
import { PageHeader } from "../../components/PageHeader";
import { SECTIONS, useAccess } from "../../state/access";

const ICONS: Record<string, ReactNode> = {
  members: <Users size={16} />,
  invitations: <Mail size={16} />,
  roles: <Shield size={16} />,
  "api-keys": <KeyRound size={16} />,
  connections: <Plug size={16} />,
  settings: <SlidersHorizontal size={16} />,
};

export function OrganizationView() {
  const access = useAccess();
  const folded = useSidebarFolded();
  const { pathname } = useLocation();
  const org = access.organization;
  const current = access.sections.find((s) => pathname.startsWith(`/organization/${s.path}`));
  return (
    <div className="page">
      <PageSidebar title="Organization" back={null} backLabel="Show the views">
        <div className="org-card">
          {org ? (
            <>
              <OrgAvatar organization={org} size={24} />
              <div className="org-card-text">
                <strong>{org.name}</strong>
                <span className="copyable">
                  <span className="digest">{org.id}</span>
                  <CopyButton text={org.id} label="Copy the organization's id" />
                </span>
              </div>
            </>
          ) : (
            <div className="org-card-text">
              <strong>Access</strong>
              {access.identity.tenant && (
                <span className="muted">
                  tenant <span className="mono">{access.identity.tenant}</span>
                </span>
              )}
            </div>
          )}
        </div>
        {access.sections.length > 0 && (
          <nav className="nav" aria-label="Organization">
            {access.sections.map((s) => (
              <NavLink
                key={s.path}
                to={`/organization/${s.path}`}
                className="nav-item"
                title={folded ? s.title : undefined}
              >
                <span className="nav-icon" aria-hidden="true">
                  {ICONS[s.path]}
                </span>
                <span className="nav-label">{s.title}</span>
              </NavLink>
            ))}
          </nav>
        )}
      </PageSidebar>
      <PageHeader crumbs={org ? org.name : "Access"} title={current?.title ?? "Organization"} />
      <div className="page-body org-body">
        <Outlet />
      </div>
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
          Members, invitations, connections and settings are those of an organization, seen by its
          members once signed in. An API key reaches its tenant's roles and keys.
        </p>
      </div>
    );
  }
  return (
    <NoPermission permission={section.permission} what={`open ${section.title.toLowerCase()}`} />
  );
}
