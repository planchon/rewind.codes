// Invitations: who was invited, with the grant they get on accepting; whoever holds
// `members:invite` invites an email with a role and allowed labels, resends and revokes.
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Send, UserPlus } from "lucide-react";
import { useState } from "react";
import { ORG_INVITATIONS, useOrgInvitations, useRoles } from "../../api/organization";
import type { Invitation, InvitationState } from "../../api/types";
import { useAccess } from "../../app/access";
import { useApi } from "../../app/session";
import { Ago, Empty, LabelList, Loading } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import {
  ConfirmButton,
  defaultRole,
  givableRoles,
  LabelsField,
  PrincipalName,
  RoleField,
  useLabels,
  useMemberNames,
} from "./bits";
import { SectionGate } from "./OrganizationView";

export function InvitationsView() {
  return (
    <SectionGate path="invitations">
      <Invitations />
    </SectionGate>
  );
}

const ORDER: Record<InvitationState, number> = { pending: 0, expired: 1, accepted: 2, revoked: 3 };

function InviteForm({ onSent }: { onSent: (invitation: Invitation) => void }) {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const roles = useRoles(access.can("roles:read"));
  const givable = roles.data ? givableRoles(roles.data, access.permissions) : null;
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<string | null>(null);
  const labels = useLabels();
  const chosen = role ?? defaultRole(givable);
  const invite = useMutation({
    mutationFn: () =>
      api.invite({ email: email.trim(), role: chosen, allowedLabels: labels.labels ?? {} }),
    onSuccess: async (invitation) => {
      await queryClient.invalidateQueries({ queryKey: ORG_INVITATIONS });
      onSent(invitation);
    },
  });
  const submit = () => {
    if (email.trim() && chosen && labels.labels) invite.mutate();
  };
  if (roles.isPending && access.can("roles:read")) return <Loading />;
  return (
    <DialogForm
      title="Invite someone"
      description="They get an email with a link to sign in and join, with this role."
      submit={
        <>
          <Send size={12} /> Send the invitation
        </>
      }
      pending={invite.isPending}
      pendingLabel="Sending…"
      disabled={!email.trim() || !chosen || !labels.labels}
      error={invite.error}
      onSubmit={submit}
    >
      <div className="form-row">
        <label className="field col grow">
          <span className="field-label">Email</span>
          <input
            className="input"
            type="email"
            placeholder="someone@company.com"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            required
          />
        </label>
        <RoleField roles={givable} value={chosen} onChange={setRole} />
      </div>
      <div className="form-row">
        <LabelsField value={labels.text} onChange={labels.setText} error={labels.error} />
      </div>
    </DialogForm>
  );
}

function Invitations() {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const invitations = useOrgInvitations();
  const names = useMemberNames();
  const canInvite = access.can("members:invite");
  const [inviting, setInviting] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [failed, setFailed] = useState<unknown>(null);
  const refresh = () => queryClient.invalidateQueries({ queryKey: ORG_INVITATIONS });
  const resend = useMutation({
    mutationFn: (id: string) => api.resendInvitation(id),
    onMutate: () => setFailed(null),
    onSuccess: (inv) => {
      setNotice(`The invitation was sent again to ${inv.email}.`);
      return refresh();
    },
    onError: setFailed,
  });
  const revoke = useMutation({
    mutationFn: (id: string) => api.revokeInvitation(id),
    onMutate: () => setFailed(null),
    onSuccess: () => {
      setNotice("The invitation was revoked.");
      return refresh();
    },
    onError: setFailed,
  });

  return (
    <section className="org-section" aria-label="Invitations">
      <div className="section-bar">
        <p className="section-intro dim">
          Who was invited, and the role and allowed labels they get on accepting.
        </p>
        {canInvite && (
          <button type="button" className="btn" onClick={() => setInviting(true)}>
            <UserPlus size={12} /> Invite someone
          </button>
        )}
      </div>
      <FormDialog open={inviting && canInvite} onOpenChange={setInviting}>
        <InviteForm
          onSent={(inv) => {
            setInviting(false);
            setNotice(`An invitation was sent to ${inv.email}.`);
          }}
        />
      </FormDialog>
      {notice && (
        <div className="notice" role="status">
          {notice}
        </div>
      )}
      {failed ? <ErrorNote error={failed} /> : null}
      {invitations.isPending ? (
        <Loading />
      ) : invitations.error ? (
        <ErrorNote error={invitations.error} />
      ) : invitations.data.length === 0 ? (
        <Empty>No invitation sent yet.</Empty>
      ) : (
        <div className="table-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Email</th>
                <th>Role</th>
                <th>Allowed labels</th>
                <th>State</th>
                <th>Invited by</th>
                <th>Sent</th>
                <th>Expires</th>
                {canInvite && <th className="right">Actions</th>}
              </tr>
            </thead>
            <tbody>
              {[...invitations.data]
                .sort(
                  (a, b) =>
                    ORDER[a.state] - ORDER[b.state] || b.createdAt.localeCompare(a.createdAt),
                )
                .map((inv) => (
                  <tr key={inv.id} className={inv.state === "pending" ? undefined : "dimmed"}>
                    <td>{inv.email}</td>
                    <td>
                      {inv.grant ? (
                        <span className="tag">{inv.grant.role}</span>
                      ) : (
                        <span className="muted">–</span>
                      )}
                    </td>
                    <td className="wrap">
                      <LabelList labels={inv.grant?.allowedLabels ?? {}} />
                    </td>
                    <td>
                      <span className={`tag invitation-${inv.state}`}>{inv.state}</span>
                    </td>
                    <td className="dim">
                      {inv.inviter ? (
                        <PrincipalName text={`user:${inv.inviter}`} names={names} />
                      ) : (
                        <span className="muted">the operator</span>
                      )}
                    </td>
                    <td className="dim">
                      <Ago ms={Date.parse(inv.createdAt)} />
                    </td>
                    <td className="dim">
                      {inv.state === "pending" || inv.state === "expired" ? (
                        <Ago ms={Date.parse(inv.expiresAt)} />
                      ) : (
                        <span className="muted">–</span>
                      )}
                    </td>
                    {canInvite && (
                      <td className="right">
                        <span className="row-actions">
                          {(inv.state === "pending" || inv.state === "expired") && (
                            <button
                              type="button"
                              className="btn small"
                              disabled={resend.isPending}
                              aria-label={`Resend the invitation to ${inv.email}`}
                              onClick={() => resend.mutate(inv.id)}
                            >
                              Resend
                            </button>
                          )}
                          {inv.state === "pending" && (
                            <ConfirmButton
                              confirm={`Revoke the invitation to ${inv.email}?`}
                              description="The link in the email stops working. Invite them again to send a new one."
                              pending={revoke.isPending}
                              onConfirm={() => revoke.mutate(inv.id)}
                            >
                              Revoke
                            </ConfirmButton>
                          )}
                        </span>
                      </td>
                    )}
                  </tr>
                ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
