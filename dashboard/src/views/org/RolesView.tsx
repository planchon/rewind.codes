// Roles: the four built-in ones, which never change, and the organization's own, composed from
// the catalog of permissions. Writing a role needs every permission it gives; a role still held
// by a grant cannot be deleted.
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { useState } from "react";
import { ApiError } from "../../api/client";
import { ROLES, usePermissionCatalog, useRoles, WHOAMI } from "../../api/organization";
import type { Permission, PermissionInfo, Role } from "../../api/types";
import { useAccess } from "../../app/access";
import { useApi } from "../../app/session";
import { Ago, Loading } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import { groupPermissions, missingPermissions, roleNameProblem } from "../../model/access";
import { ConfirmButton, Described, Field, PermissionList } from "./bits";
import { SectionGate } from "./OrganizationView";

export function RolesView() {
  return (
    <SectionGate path="roles">
      <Roles />
    </SectionGate>
  );
}

/** Creates a role (`role` absent) or changes one: its description and its permissions. */
export function RoleEditor({
  role,
  catalog,
  existing,
  onDone,
}: {
  role: Role | null;
  catalog: PermissionInfo[];
  /** The names already taken. */
  existing: readonly string[];
  onDone: () => void;
}) {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const [name, setName] = useState(role?.name ?? "");
  const [description, setDescription] = useState(role?.description ?? "");
  const [chosen, setChosen] = useState<ReadonlySet<Permission>>(
    () => new Set(role?.permissions ?? []),
  );
  const [touched, setTouched] = useState(false);
  const nameProblem = role
    ? null
    : (roleNameProblem(name) ??
      (existing.includes(name) ? `The organization already has a role ${name}.` : null));
  const save = useMutation({
    mutationFn: () =>
      api.putRole(role?.name ?? name, {
        description: description.trim(),
        // In the catalog's order.
        permissions: catalog.map((c) => c.permission).filter((p) => chosen.has(p)),
      }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ROLES });
      // A change applies at once to whoever holds the role, the caller included.
      await queryClient.invalidateQueries({ queryKey: WHOAMI });
      onDone();
    },
  });
  const toggle = (p: Permission) =>
    setChosen((before) => {
      const next = new Set(before);
      if (next.has(p)) next.delete(p);
      else next.add(p);
      return next;
    });
  const submit = () => {
    setTouched(true);
    if (!nameProblem) save.mutate();
  };

  return (
    <DialogForm
      title={role ? `Edit the role ${role.name}` : "New role"}
      meta={`${chosen.size} of ${catalog.length} permissions`}
      submit={role ? "Save the role" : "Create the role"}
      pending={save.isPending}
      disabled={touched && nameProblem !== null}
      error={save.error}
      onSubmit={submit}
    >
      <div className="form-row">
        <Field
          label="Name"
          hint="Lowercase letters, digits, - and _; at most 64."
          error={touched ? nameProblem : null}
        >
          {(props) => (
            <input
              {...props}
              className="input mono"
              value={name}
              disabled={role !== null}
              placeholder="auditor"
              spellCheck={false}
              onChange={(e) => setName(e.target.value)}
              onBlur={() => setTouched(true)}
            />
          )}
        </Field>
        <Field label="Description" hint="Shown wherever the role is given." grow>
          {(props) => (
            <input
              {...props}
              className="input"
              value={description}
              placeholder="What the role is for"
              onChange={(e) => setDescription(e.target.value)}
            />
          )}
        </Field>
      </div>
      <fieldset className="perm-groups">
        <legend className="field-label">
          Permissions <span className="muted">— you can give only those you hold yourself</span>
        </legend>
        {groupPermissions(catalog).map((group) => (
          <div key={group.resource} className="perm-group">
            <h3>{group.resource.replace("_", " ")}</h3>
            {group.items.map((item) => {
              const held = access.permissions.has(item.permission);
              return (
                <label
                  key={item.permission}
                  className={held ? "perm-check" : "perm-check unheld"}
                  title={held ? undefined : "You do not hold this permission"}
                >
                  <input
                    type="checkbox"
                    checked={chosen.has(item.permission)}
                    disabled={!held}
                    onChange={() => toggle(item.permission)}
                  />
                  <span className="perm-name mono">{item.permission}</span>
                  <span className="perm-desc">
                    <Described text={item.description} />
                  </span>
                </label>
              );
            })}
          </div>
        ))}
      </fieldset>
    </DialogForm>
  );
}

function Roles() {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const roles = useRoles();
  const write = access.can("roles:write");
  const catalog = usePermissionCatalog(write);
  // The role in the dialog (`null` for a new one), kept while it closes.
  const [editing, setEditing] = useState(false);
  const [target, setTarget] = useState<Role | null>(null);
  const edit = (role: Role | null) => {
    setTarget(role);
    setEditing(true);
  };
  const [failed, setFailed] = useState<{ role: string; error: unknown } | null>(null);
  const remove = useMutation({
    mutationFn: (name: string) => api.deleteRole(name),
    onMutate: () => setFailed(null),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ROLES }),
    onError: (error, role) => setFailed({ role, error }),
  });

  if (roles.isPending) return <Loading />;
  if (roles.error) return <ErrorNote error={roles.error} />;
  const builtIn = roles.data.filter((r) => r.builtIn);
  const own = roles.data.filter((r) => !r.builtIn);

  const row = (r: Role) => {
    const beyond = missingPermissions(r.permissions, access.permissions);
    const locked =
      beyond.length > 0 ? `It gives ${beyond.join(", ")}, which you do not hold.` : undefined;
    return (
      <li key={r.name} className="role-row" aria-label={`Role ${r.name}`}>
        <div className="role-head">
          <span className="role-name mono">{r.name}</span>
          {r.builtIn && <span className="tag">built-in</span>}
          {r.name === access.role && <span className="tag info">your role</span>}
          <span className="role-desc dim">{r.description}</span>
          {!r.builtIn && r.updatedAt && (
            <span className="muted role-date">
              changed <Ago ms={Date.parse(r.updatedAt)} />
            </span>
          )}
          {write && !r.builtIn && (
            <span className="row-actions">
              <button
                type="button"
                className="btn small"
                disabled={locked !== undefined || catalog.isPending}
                title={locked}
                aria-label={`Edit the role ${r.name}`}
                onClick={() => edit(r)}
              >
                Edit
              </button>
              <ConfirmButton
                confirm={`Delete the role ${r.name}?`}
                pending={remove.isPending}
                onConfirm={() => remove.mutate(r.name)}
              >
                Delete
              </ConfirmButton>
            </span>
          )}
        </div>
        <PermissionList permissions={r.permissions} />
        {failed?.role === r.name && (
          <ErrorNote
            error={failed.error}
            title={
              failed.error instanceof ApiError && failed.error.slug === "role-in-use"
                ? `${r.name} is still given`
                : undefined
            }
          />
        )}
      </li>
    );
  };

  return (
    <section className="org-section" aria-label="Roles">
      <div className="section-bar">
        <p className="section-intro dim">
          A role is a set of permissions. The built-in roles never change; changing one of the
          organization's applies at once to everyone holding it.
        </p>
        {write && (
          <button
            type="button"
            className="btn"
            disabled={catalog.isPending}
            onClick={() => edit(null)}
          >
            <Plus size={12} /> New role
          </button>
        )}
      </div>
      {catalog.error && <ErrorNote error={catalog.error} />}
      <FormDialog open={editing && catalog.data !== undefined} onOpenChange={setEditing} wide>
        {catalog.data && (
          <RoleEditor
            role={target}
            catalog={catalog.data}
            existing={roles.data.map((r) => r.name)}
            onDone={() => setEditing(false)}
          />
        )}
      </FormDialog>
      <div className="panel">
        <div className="panel-head">
          <h2>The organization's roles</h2>
          <span className="end muted">{own.length}</span>
        </div>
        {own.length === 0 ? (
          <p className="panel-body muted">
            None yet.{write ? " Compose one from the catalog with New role." : ""}
          </p>
        ) : (
          <ul className="role-list">{own.map(row)}</ul>
        )}
      </div>
      <div className="panel">
        <div className="panel-head">
          <h2>Built-in roles</h2>
        </div>
        <ul className="role-list">{builtIn.map(row)}</ul>
      </div>
    </section>
  );
}
