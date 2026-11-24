// Members: the organization's memberships, each with its grant (role, allowed labels). Whoever
// holds `members:manage` changes a grant or removes a member, within what they hold themselves.
import { Fragment, useState } from "react";
import { Ago, Empty, Loading } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import { RowLabels } from "../../components/RowLabels";
import { TableScroll } from "../../components/TableScroll";
import { displayName } from "../../model/access";
import type { Member, Role } from "../../model/types";
import { useAccess } from "../../state/access";
import { useMembers, useRemoveMember, useRoles, useSetGrant } from "../../state/organization";
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
  roles: readonly Role[] | null;
  onDone: () => void;
}) {
  const access = useAccess();
  const givable = roles ? givableRoles(roles, access.permissions) : null;
  const current = member.grant?.role;
  const [role, setRole] = useState(
    current && (!givable || givable.some((r) => r.name === current))
      ? current
      : defaultRole(givable),
  );
  const labels = useLabels(member.grant?.allowedLabels);
  // The member shows the new role at once; a refusal takes it back, and says why here.
  const save = useSetGrant();
  return (
    <DialogForm
      title={`Role of ${displayName(member.user)}`}
      description="What they may do in the organization, and the labels of the runs they see."
      submit="Save"
      pending={save.pending}
      disabled={!role || !labels.labels}
      error={save.error}
      onSubmit={() => {
        if (!labels.labels) return;
        void save.run(
          {
            user: member.user.id,
            grant: { role, allowedLabels: labels.labels },
            self: member.user.id === access.user?.id,
          },
          onDone,
        );
      }}
    >
      <div className="form-row">
        <RoleField roles={givable} value={role} onChange={setRole} />
        <LabelsField value={labels.text} onChange={labels.setText} error={labels.error} />
      </div>
    </DialogForm>
  );
}

function Members() {
  const access = useAccess();
  const members = useMembers();
  const roles = useRoles(access.can("roles:read"));
  const names = useMemberNames();
  // The member in the dialog, kept while it closes.
  const [editing, setEditing] = useState(false);
  const [target, setTarget] = useState<Member | null>(null);
  const manage = access.can("members:manage");
  // The member goes at once; a refusal brings them back, with the reason under their row.
  const remove = useRemoveMember();
  const [removing, setRemoving] = useState<string | null>(null);
  const failed = remove.error && removing ? { user: removing, error: remove.error } : null;

  if (members.isPending) return <Loading />;
  if (members.isError) return <ErrorNote error={members.error} />;
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
        <TableScroll>
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
                        <RowLabels labels={m.grant?.allowedLabels ?? {}} empty />
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
                              onClick={() => {
                                setTarget(m);
                                setEditing(true);
                              }}
                            >
                              Change role
                            </button>
                            <ConfirmButton
                              confirm={
                                you ? "Leave the organization?" : `Remove ${displayName(m.user)}?`
                              }
                              disabled={locked !== undefined}
                              title={locked}
                              description={
                                you
                                  ? "You lose access to the organization at once, with your connections' access to it. An invitation brings you back."
                                  : "They lose access to the organization at once, with their connections' access to it. An invitation brings them back."
                              }
                              pending={remove.pending}
                              onConfirm={() => {
                                setRemoving(m.user.id);
                                void remove.run(m.user.id);
                              }}
                            >
                              {you ? "Leave" : "Remove"}
                            </ConfirmButton>
                          </span>
                        </td>
                      )}
                    </tr>
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
        </TableScroll>
      )}
      <FormDialog open={editing} onOpenChange={setEditing}>
        {target && (
          <GrantEditor
            member={target}
            roles={roles.data ?? null}
            onDone={() => setEditing(false)}
          />
        )}
      </FormDialog>
    </section>
  );
}
