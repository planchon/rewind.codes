// Settings: the organization's name, and the role a member gets at their first session when no
// invitation gave one.
import { type FormEvent, useState } from "react";
import { Loading } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import type { OrganizationSettings } from "../../model/types";
import { useAccess } from "../../state/access";
import { useRoles, useUpdateOrganization } from "../../state/organization";
import { RoleField } from "./bits";
import { SectionGate } from "./OrganizationView";

export function SettingsView() {
  return (
    <SectionGate path="settings">
      <Settings />
    </SectionGate>
  );
}

function Settings() {
  const access = useAccess();
  const roles = useRoles(access.can("roles:read"));
  const organization = access.organization;
  if (!organization) return null;
  if (roles.isPending && access.can("roles:read")) return <Loading />;
  return <SettingsForm organization={organization} roles={roles.data ?? null} />;
}

function SettingsForm({
  organization,
  roles,
}: {
  organization: OrganizationSettings;
  roles: Parameters<typeof RoleField>[0]["roles"];
}) {
  const [name, setName] = useState(organization.name);
  const [defaultRole, setDefaultRole] = useState(organization.defaultRole);
  const [saved, setSaved] = useState(false);
  // The name shows in the switcher, the default role here: both come with who calls, read again.
  const save = useUpdateOrganization();
  const changed = name.trim() !== organization.name || defaultRole !== organization.defaultRole;
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!name.trim() || !changed) return;
    setSaved(false);
    void save.run(
      {
        ...(name.trim() !== organization.name ? { name: name.trim() } : {}),
        ...(defaultRole !== organization.defaultRole ? { defaultRole } : {}),
      },
      () => setSaved(true),
    );
  };
  return (
    <form className="panel settings-form" aria-label="Settings" onSubmit={submit}>
      <div className="panel-head">
        <h2>Settings</h2>
      </div>
      <div className="panel-body stack">
        <label className="field col">
          <span className="field-label">Name</span>
          <input className="input" value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <div>
          <RoleField roles={roles} value={defaultRole} onChange={setDefaultRole} />
          <p className="field-hint">
            The role a member gets at their first session in the organization, when no invitation
            gave one. A member's role can be changed afterwards on the Members page.
          </p>
        </div>
        {save.error ? <ErrorNote error={save.error} /> : null}
        {saved && !changed && (
          <div className="notice" role="status">
            Saved.
          </div>
        )}
        <div className="form-actions">
          <button
            type="submit"
            className="btn primary"
            disabled={!name.trim() || !changed || save.pending}
          >
            {save.pending ? "Saving…" : "Save"}
          </button>
        </div>
      </div>
    </form>
  );
}
