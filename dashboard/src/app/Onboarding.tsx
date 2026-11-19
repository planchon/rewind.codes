// A member signed in without an organization: join one they were invited to, open one they
// belong to, or create one. Without an organization, the access token reaches only the onboarding
// endpoints.
import { useMutation } from "@tanstack/react-query";
import { Building2, Mail, Plus } from "lucide-react";
import { type FormEvent, useState } from "react";
import { useMyInvitations, useWhoami } from "../api/organization";
import { Ago, Loading } from "../components/bits";
import { ErrorNote } from "../components/ErrorNote";
import { displayName } from "../model/access";
import { AccountMenu, useJoinOrCreate } from "./AccountMenus";
import { Brand } from "./Brand";
import { atConsent } from "./pendingRoute";
import { useSession } from "./session";

export function Onboarding() {
  const { phase, switchOrganization } = useSession();
  const invitations = useMyInvitations();
  const whoami = useWhoami();
  const { join, create } = useJoinOrCreate();
  const open = useMutation({ mutationFn: (id: string) => switchOrganization(id) });
  const [name, setName] = useState("");
  if (phase.kind !== "member") return null;
  const user = phase.state.user;
  const pending = join.isPending || create.isPending || open.isPending;
  const organizations = whoami.data?.organizations ?? [];

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const trimmed = name.trim();
    if (trimmed) create.mutate(trimmed);
  };

  return (
    <div className="onboarding-shell">
      <header className="onboarding-bar">
        <Brand />
        <div>
          <AccountMenu user={user} />
        </div>
      </header>
      <main className="onboarding">
        <div className="onboarding-head">
          <h1>Welcome, {displayName(user)}</h1>
          <p className="dim">
            Runs, scripts and keys belong to an organization. Join one you were invited to, or
            create your own: you will be its owner.
          </p>
        </div>
        {atConsent() && (
          <p className="gate-note" role="note">
            Claude is waiting for your answer. Open, join or create an organization first: you then
            choose what Claude may open in it.
          </p>
        )}

        {organizations.length > 0 && (
          <section className="panel" aria-labelledby="your-organizations">
            <div className="panel-head">
              <h2 id="your-organizations">Your organizations</h2>
            </div>
            <ul className="choices panel-body">
              {organizations.map((o) => (
                <li key={o.id}>
                  <button
                    type="button"
                    className="choice"
                    disabled={pending}
                    onClick={() => open.mutate(o.id)}
                  >
                    <Building2 size={15} aria-hidden="true" />
                    <span className="choice-name">Open {o.name}</span>
                    <span className="mono muted choice-id">{o.id}</span>
                  </button>
                </li>
              ))}
            </ul>
            {open.error && (
              <div className="panel-body">
                <ErrorNote error={open.error} />
              </div>
            )}
          </section>
        )}

        <section className="panel" aria-labelledby="invitations-waiting">
          <div className="panel-head">
            <Mail size={13} aria-hidden="true" />
            <h2 id="invitations-waiting">Invitations</h2>
            <span className="end muted">sent to {user.email}</span>
          </div>
          <div className="panel-body">
            {invitations.isPending ? (
              <Loading />
            ) : invitations.error ? (
              <ErrorNote error={invitations.error} />
            ) : invitations.data.length === 0 ? (
              <p className="muted onboarding-empty">
                No invitation is waiting for {user.email}. A member of an organization can invite
                you from its Members page.
              </p>
            ) : (
              <ul className="choices">
                {invitations.data.map((inv) => (
                  <li key={inv.id} className="invitation-choice">
                    <div className="invitation-text">
                      <strong>{inv.organization?.name ?? "An organization"}</strong>
                      <span className="muted">
                        {inv.grant ? (
                          <>
                            as <span className="tag">{inv.grant.role}</span>
                          </>
                        ) : null}{" "}
                        · expires <Ago ms={Date.parse(inv.expiresAt)} />
                      </span>
                    </div>
                    <button
                      type="button"
                      className="btn primary"
                      disabled={pending}
                      onClick={() => join.mutate(inv)}
                    >
                      Join {inv.organization?.name ?? "it"}
                    </button>
                  </li>
                ))}
              </ul>
            )}
            {join.error && <ErrorNote error={join.error} />}
          </div>
        </section>

        <section className="panel" aria-labelledby="create-organization">
          <div className="panel-head">
            <Plus size={13} aria-hidden="true" />
            <h2 id="create-organization">Create an organization</h2>
          </div>
          <form className="panel-body form-row" onSubmit={submit}>
            <label className="field col grow">
              <span className="field-label">Name</span>
              <input
                className="input"
                placeholder="Acme"
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
            </label>
            <button type="submit" className="btn primary" disabled={!name.trim() || pending}>
              {create.isPending ? "Creating…" : "Create"}
            </button>
          </form>
          {create.error && (
            <div className="panel-body">
              <ErrorNote error={create.error} />
            </div>
          )}
        </section>
      </main>
    </div>
  );
}
