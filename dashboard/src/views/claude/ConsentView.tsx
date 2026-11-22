// The consent of an MCP client (archi/07, "MCP and OAuth"): `/oauth/authorize` sends the browser
// to `/connect/authorize?request=…`, signed in or not (the sign-in comes back to this address).
// The person ticks the organizations the client may open, gives each an access within their own
// role, keeps or unticks the labels of what it starts, and allows or denies; the browser then
// goes back to the client. A page of its own, outside the dashboard's frame, as a sign-in is.
import { CircleAlert, Laptop, ShieldQuestion } from "lucide-react";
import { type ReactNode, useState } from "react";
import { useSearchParams } from "react-router";
import { Brand } from "../../app/Brand";
import { Loading, ProblemView, useNow } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import { displayName } from "../../model/access";
import {
  ACCESS_INFO,
  ACCESSES,
  type ConsentChoice,
  hostOf,
  initialChoices,
  labelsText,
} from "../../model/connections";
import { formatRelative } from "../../model/format";
import type { ConnectionAccess, ConsentRequest } from "../../model/types";
import { useAnswerConsent, useConsent } from "../../state/connections";
import { ApiError } from "../../state/errors";
import { useSession } from "../../state/session";

export function ConsentView() {
  const [params] = useSearchParams();
  return <Consent request={params.get("request")} />;
}

/** A refusal that means the request is gone: expired, answered, or never valid. */
function isGone(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    [400, 404, 410].includes(error.status) &&
    error.slug !== "forbidden"
  );
}

function Card({ children, label }: { children: ReactNode; label: string }) {
  return (
    <div className="gate">
      <section className="gate-card consent-card" aria-label={label}>
        <Brand />
        {children}
      </section>
    </div>
  );
}

function Expired() {
  return (
    <Card label="Request expired">
      <h1>This request has expired</h1>
      <p>
        An authorization request lasts 10 minutes and is answered once. Go back to Claude and
        connect Rewind again: it opens a new one.
      </p>
    </Card>
  );
}

export function Consent({ request }: { request: string | null }) {
  const consent = useConsent(request);
  if (!request) return <Expired />;
  if (consent.isPending) {
    return (
      <div className="gate">
        <Loading>Reading the request…</Loading>
      </div>
    );
  }
  if (consent.isError) {
    if (isGone(consent.error)) return <Expired />;
    return (
      <Card label="The request could not be read">
        <ProblemView error={consent.error} title="The request could not be read" />
        <div className="gate-actions">
          <button type="button" className="btn" onClick={() => void consent.refresh()}>
            Try again
          </button>
        </div>
      </Card>
    );
  }
  return <ConsentForm request={request} consent={consent.data} />;
}

function ConsentForm({ request, consent }: { request: string; consent: ConsentRequest }) {
  const { phase, assign } = useSession();
  const now = useNow(1_000);
  const skew = phase.kind === "member" ? phase.skewMs : 0;
  const [choices, setChoices] = useState(() => initialChoices(consent));
  const hasLabels = Object.keys(consent.allowedLabels).length > 0;
  const [onlyLabelled, setOnlyLabelled] = useState(true);
  const [leaving, setLeaving] = useState<string | null>(null);
  const answer = useAnswerConsent();
  const decide = (decision: "allow" | "deny") =>
    void answer.run(
      {
        request,
        decision,
        organizations:
          decision === "allow"
            ? consent.organizations
                .filter((o) => choices.get(o.id)?.checked)
                .map((o) => ({ id: o.id, access: choices.get(o.id)?.access ?? "read" }))
            : [],
        allowedLabels: decision === "allow" && onlyLabelled ? consent.allowedLabels : {},
      },
      ({ redirectTo }) => {
        setLeaving(hostOf(redirectTo) ?? consent.redirectHost);
        assign(redirectTo);
      },
    );

  const expiresAt = Date.parse(consent.expiresAt);
  if (!leaving && expiresAt <= now + skew) return <Expired />;

  const set = (id: string, next: Partial<ConsentChoice>) =>
    setChoices((current) => {
      const copy = new Map(current);
      const before = copy.get(id) ?? { checked: false, access: "read" as ConnectionAccess };
      copy.set(id, { ...before, ...next });
      return copy;
    });
  const ticked = consent.organizations.filter((o) => choices.get(o.id)?.checked).length;
  const busy = answer.pending || leaving !== null;
  const client = consent.client;
  const site = hostOf(client.uri ?? null);
  const user = phase.kind === "member" ? phase.state.user : null;

  if (leaving) {
    return (
      <Card label="Returning to the client">
        <h1>Returning to {client.name}…</h1>
        <p>
          Your browser goes back to <strong>{leaving}</strong>. You can close this page if it does
          not.
        </p>
      </Card>
    );
  }

  return (
    <Card label={`${client.name} wants to access Rewind`}>
      <div className="consent-head">
        <h1>{client.name} wants to access Rewind</h1>
        <p className="consent-meta">
          {site && (
            <>
              <span>{site}</span>
              <span aria-hidden="true"> · </span>
            </>
          )}
          <span>
            returns to <strong>{consent.redirectHost}</strong>
          </span>
        </p>
        {user && (
          <p className="consent-meta">
            Signed in as <strong>{displayName(user)}</strong> ({user.email})
          </p>
        )}
      </div>

      {consent.loopback && (
        <p className="consent-warning" role="note">
          <Laptop size={14} aria-hidden="true" />
          <span>
            It returns to a program on this computer ({consent.redirectHost}). Allow it only if you
            started it yourself, from Claude Code for instance.
          </span>
        </p>
      )}
      {client.kind === "registered" && (
        <p className="consent-warning" role="note">
          <ShieldQuestion size={14} aria-hidden="true" />
          <span>
            It registered itself with Rewind: its name is its own claim. Allow it only if you just
            connected it.
          </span>
        </p>
      )}

      <fieldset className="consent-orgs">
        <legend className="field-label">Organizations it may open</legend>
        {consent.organizations.length === 0 ? (
          <p className="muted">
            You belong to no organization yet: create one in the dashboard, then connect again.
          </p>
        ) : (
          consent.organizations.map((o) => {
            const choice = choices.get(o.id) ?? { checked: false, access: "read" as const };
            const none = o.accesses.length === 0;
            const name = o.name ?? o.id;
            return (
              <div key={o.id} className={none ? "consent-org disabled" : "consent-org"}>
                <label className="consent-org-check">
                  <input
                    type="checkbox"
                    checked={choice.checked}
                    disabled={none || busy}
                    onChange={(e) => set(o.id, { checked: e.target.checked })}
                  />
                  <span className="consent-org-name">{name}</span>
                  {o.role && <span className="badge gray">{o.role}</span>}
                </label>
                {none ? (
                  <span className="field-hint">Your role here gives neither access.</span>
                ) : (
                  <select
                    className="input"
                    aria-label={`Access in ${name}`}
                    value={choice.access}
                    disabled={busy}
                    onChange={(e) =>
                      set(o.id, { access: e.target.value as ConnectionAccess, checked: true })
                    }
                  >
                    {ACCESSES.map((a) => (
                      <option key={a} value={a} disabled={!o.accesses.includes(a)}>
                        {ACCESS_INFO[a].title}
                      </option>
                    ))}
                  </select>
                )}
              </div>
            );
          })
        )}
        <dl className="access-list small">
          <dt>Read</dt>
          <dd>{ACCESS_INFO.read.gives}.</dd>
          <dt>Run</dt>
          <dd>{ACCESS_INFO.run.gives}.</dd>
        </dl>
      </fieldset>

      {hasLabels && (
        <div className="stack tight">
          <label className="check">
            <input
              type="checkbox"
              checked={onlyLabelled}
              disabled={busy}
              onChange={(e) => setOnlyLabelled(e.target.checked)}
            />
            Only what {client.name} starts ({labelsText(consent.allowedLabels)})
          </label>
          <span className="field-hint">
            The runs it starts carry these labels, and it sees only runs carrying them. Untick it to
            let it read and steer the organization's other runs too.
          </span>
        </div>
      )}

      <p className="consent-meta">
        {client.name} never gets more than your own role gives, and never administers anything. You
        can change or revoke it from Account › Connections.
      </p>

      {answer.error ? <ErrorNote error={answer.error} /> : null}

      <div className="consent-actions">
        <span className="muted consent-expiry">
          <CircleAlert size={12} aria-hidden="true" /> Expires{" "}
          {formatRelative(expiresAt, now + skew)}
        </span>
        <button type="button" className="btn" disabled={busy} onClick={() => decide("deny")}>
          Deny
        </button>
        <button
          type="button"
          className="btn primary"
          disabled={busy || ticked === 0}
          onClick={() => decide("allow")}
        >
          Allow
        </button>
      </div>
    </Card>
  );
}
