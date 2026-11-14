// Members: the organization's memberships, each with its grant (role, allowed labels). Whoever
// holds `members:manage` changes a grant or removes a member, within what they hold themselves.
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Fragment, useState } from "react";
import { MEMBERS, useMembers, useRoles, WHOAMI } from "../../api/organization";
import type { Member, Role } from "../../api/types";
import { useAccess } from "../../app/access";
import { useApi } from "../../app/session";
import { Ago, Empty, LabelList, Loading } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import { displayName } from "../../model/access";
import {
  beyondCaller,
  ConfirmButton,
  defaultRole,
  givableRoles,
  LabelsField,
  Person,
  PrincipalName,
  RoleField,
  useLabels,
  useMemberNames,
} from "./bits";
import { SectionGate } from "./OrganizationView";

export function MembersView() {
  return (
    <SectionGate path="members">
      <Members />
    </SectionGate>
  );
}

function GrantEditor({
  member,
  roles,
  onDone,
}: {
  member: Member;
  roles: Role[] | null;
  onDone: () => void;
}) {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const givable = roles ? givableRoles(roles, access.permissions) : null;
  const current = member.grant?.role;
  const [role, setRole] = useState(
    current && (!givable || givable.some((r) => r.name === current))
      ? current
      : defaultRole(givable),
  );
  const labels = useLabels(member.grant?.allowedLabels);
  const save = useMutation({
    mutationFn: () => api.setGrant(member.user.id, { role, allowedLabels: labels.labels ?? {} }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: MEMBERS });
      if (member.user.id === access.user?.id)
        await queryClient.invalidateQueries({ queryKey: WHOAMI });
      onDone();
    },
  });
  return (
    <form
      className="inline-form"
      aria-label={`Role of ${displayName(member.user)}`}
      onSubmit={(e) => {
        e.preventDefault();
        if (labels.labels) save.mutate();
      }}
    >
      <div className="form-row">
        <RoleField roles={givable} value={role} onChange={setRole} />
        <LabelsField value={labels.text} onChange={labels.setText} error={labels.error} />
        <div className="form-actions">
          <button
            type="submit"
            className="btn primary"
            disabled={!role || !labels.labels || save.isPending}
          >
            {save.isPending ? "Saving…" : "Save"}
          </button>
          <button type="button" className="btn ghost" onClick={onDone}>
            Cancel
          </button>
        </div>
      </div>
      {save.error && <ErrorNote error={save.error} />}
    </form>
  );
}

function Members() {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const members = useMembers();
  const roles = useRoles(access.can("roles:read"));
  const names = useMemberNames();
  const [editing, setEditing] = useState<string | null>(null);
  const [failed, setFailed] = useState<{ user: string; error: unknown } | null>(null);
  const manage = access.can("members:manage");
  const remove = useMutation({
    mutationFn: (user: string) => api.removeMember(user),
    onMutate: () => setFailed(null),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: MEMBERS }),
    onError: (error, user) => setFailed({ user, error }),
  });

  if (members.isPending) return <Loading />;
  if (members.error) return <ErrorNote error={members.error} />;
  const list = [...members.data].sort((a, b) =>
    displayName(a.user).localeCompare(displayName(b.user)),
  );
  const columns = manage ? 6 : 5;

  return (
    <section className="org-section" aria-label="Members">
      <p className="section-intro dim">
        Everyone in {access.organization?.name}, with the role their grant gives.
        {manage
          ? " You may change a grant or remove a member when you hold every permission of their role."
          : ""}
      </p>
      {list.length === 0 ? (
        <Empty>No member yet.</Empty>
      ) : (
        <div className="table-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Member</th>
                <th>Status</th>
                <th>Role</th>
                <th>Allowed labels</th>
                <th>Granted</th>
                {manage && <th className="right">Actions</th>}
              </tr>
            </thead>
            <tbody>
              {list.map((m) => {
                const you = m.user.id === access.user?.id;
                const beyond =
                  m.grant && roles.data
                    ? beyondCaller(m.grant.role, roles.data, access.permissions)
                    : [];
                const locked =
                  beyond.length > 0
                    ? `Their role gives ${beyond.join(", ")}, which you do not hold.`
                    : undefined;
                return (
                  <Fragment key={m.user.id}>
                    <tr>
                      <td>
                        <Person member={m} you={you} />
                      </td>
                      <td>
                        <span className={`tag member-${m.status}`}>{m.status}</span>
                      </td>
                      <td>
                        {m.grant ? (
                          <span className="tag">{m.grant.role}</span>
                        ) : (
                          <span
                            className="muted"
                            title="A member gets a role at their first session"
                          >
                            no role
                          </span>
                        )}
                      </td>
                      <td className="wrap">
                        <LabelList labels={m.grant?.allowedLabels ?? {}} />
                      </td>
                      <td className="dim">
                        {m.grant?.grantedAt ? (
                          <>
                            <Ago ms={Date.parse(m.grant.grantedAt)} /> by{" "}
                            <PrincipalName text={m.grant.grantedBy} names={names} />
                          </>
                        ) : (
                          <span className="muted">–</span>
                        )}
                      </td>
                      {manage && (
                        <td className="right">
                          <span className="row-actions">
                            <button
                              type="button"
                              className="btn small"
                              disabled={locked !== undefined}
                              title={locked}
                              aria-label={`Change the role of ${displayName(m.user)}`}
                              onClick={() => setEditing(editing === m.user.id ? null : m.user.id)}
                            >
                              Change role
                            </button>
                            <ConfirmButton
                              confirm={
                                you ? "Leave the organization?" : `Remove ${displayName(m.user)}?`
                              }
                              disabled={locked !== undefined}
                              title={locked}
                              pending={remove.isPending}
                              onConfirm={() => remove.mutate(m.user.id)}
                            >
                              {you ? "Leave" : "Remove"}
                            </ConfirmButton>
                          </span>
                        </td>
                      )}
                    </tr>
                    {editing === m.user.id && (
                      <tr className="editing">
                        <td colSpan={columns}>
                          <GrantEditor
                            member={m}
                            roles={roles.data ?? null}
                            onDone={() => setEditing(null)}
                          />
                        </td>
                      </tr>
                    )}
                    {failed?.user === m.user.id && (
                      <tr className="editing">
                        <td colSpan={columns}>
                          <ErrorNote error={failed.error} />
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
