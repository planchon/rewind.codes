// Invitations: who was invited, with the grant they get on accepting; whoever holds
// `members:invite` invites an email with a role and allowed labels, resends and revokes.
import { Send, UserPlus } from "lucide-react";
import { useState } from "react";
import { Ago, Empty, LabelList, Loading } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import type { Invitation, InvitationState } from "../../model/types";
import { useAccess } from "../../state/access";
import {
  useInvite,
  useOrgInvitations,
  useResendInvitation,
  useRevokeInvitation,
  useRoles,
} from "../../state/organization";
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
  const access = useAccess();
  const roles = useRoles(access.can("roles:read"));
  const givable = roles.data ? givableRoles(roles.data, access.permissions) : null;
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<string | null>(null);
  const labels = useLabels();
  const chosen = role ?? defaultRole(givable);
  const invite = useInvite();
  const submit = () => {
    if (email.trim() && chosen && labels.labels)
      void invite.run({ email: email.trim(), role: chosen, allowedLabels: labels.labels }, onSent);
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
      pending={invite.pending}
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
  const access = useAccess();
  const invitations = useOrgInvitations();
  const names = useMemberNames();
  const canInvite = access.can("members:invite");
  const [inviting, setInviting] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const resend = useResendInvitation();
  // Shown revoked at once; a refusal takes it back.
  const revoke = useRevokeInvitation();
  /** The action last asked for, whose failure shows. */
  const [last, setLast] = useState<"resend" | "revoke" | null>(null);
  const failed = last === "resend" ? resend.error : last === "revoke" ? revoke.error : null;

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
      ) : invitations.isError ? (
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
                              disabled={resend.pending}
                              aria-label={`Resend the invitation to ${inv.email}`}
                              onClick={() => {
                                setLast("resend");
                                void resend.run(inv.id, (sent) =>
                                  setNotice(`The invitation was sent again to ${sent.email}.`),
                                );
                              }}
                            >
                              Resend
                            </button>
                          )}
                          {inv.state === "pending" && (
                            <ConfirmButton
                              confirm={`Revoke the invitation to ${inv.email}?`}
                              description="The link in the email stops working. Invite them again to send a new one."
                              pending={revoke.pending}
                              onConfirm={() => {
                                setLast("revoke");
                                void revoke.run(inv.id, () =>
                                  setNotice("The invitation was revoked."),
                                );
                              }}
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
