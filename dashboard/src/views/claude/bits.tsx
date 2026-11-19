// Pieces the Claude pages share: a client's name and where it comes from, an access as a tag,
// the organizations a connection opens, and a block of text to copy.
import { BadgeCheck, Check, CircleHelp, Copy } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import type {
  Connection,
  ConnectionAccess,
  ConnectionClient,
  ConnectionOrganization,
} from "../../api/connections";
import { LabelList } from "../../components/bits";
import { ACCESS_INFO, hostOf } from "../../model/connections";

/** A client's name, with its site; one that registered itself is said to be unverified. */
export function ClientName({ client }: { client: ConnectionClient }) {
  const host = hostOf(client.uri);
  const verified = client.kind === "metadata_document";
  return (
    <span className="client-name">
      <span className="client-title">
        {client.name}
        {verified ? (
          <BadgeCheck
            size={13}
            className="tone-good"
            aria-label="Its name is published at its own address"
          />
        ) : (
          <CircleHelp
            size={13}
            className="tone-neutral"
            aria-label="It registered itself: Rewind cannot vouch for its name"
          />
        )}
      </span>
      {host && <span className="muted">{host}</span>}
    </span>
  );
}

export function AccessTag({ access }: { access: ConnectionAccess }) {
  return (
    <span
      className={access === "run" ? "tag info" : "tag"}
      title={`${ACCESS_INFO[access].title}: ${ACCESS_INFO[access].gives}`}
    >
      {ACCESS_INFO[access].title}
    </span>
  );
}

/** The organizations a connection opens, each with its access and labels. */
export function OrganizationList({
  organizations,
}: {
  organizations: readonly ConnectionOrganization[];
}) {
  if (organizations.length === 0) return <span className="muted">No organization</span>;
  return (
    <ul className="conn-orgs">
      {organizations.map((o) => (
        <li key={o.id}>
          <span className="conn-org-name">{o.name ?? o.id}</span>
          <AccessTag access={o.access} />
          {Object.keys(o.allowedLabels).length > 0 && <LabelList labels={o.allowedLabels} />}
        </li>
      ))}
    </ul>
  );
}

/** Text to copy as it is, with a button that says so: a URL, a command, a message. */
export function CopyBlock({
  text,
  label,
  wrap = false,
}: {
  text: string;
  /** What the button copies, for assistive technologies: `Copy the URL`. */
  label: string;
  /** A sentence wraps; a command or a URL stays on its line. */
  wrap?: boolean;
}) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 1_500);
    return () => clearTimeout(t);
  }, [copied]);
  return (
    <div className={wrap ? "copy-block wrap" : "copy-block"}>
      <code>{text}</code>
      <button
        type="button"
        className="btn small"
        aria-label={label}
        onClick={() =>
          void navigator.clipboard?.writeText(text).then(
            () => setCopied(true),
            () => undefined,
          )
        }
      >
        {copied ? <Check size={12} /> : <Copy size={12} />} {copied ? "Copied" : "Copy"}
      </button>
    </div>
  );
}

/** A connection's name in a sentence: its client, and the person when it is someone else's. */
export function connectionName(connection: Connection, withPerson = false): string {
  const who = connection.person.name ?? connection.person.email ?? connection.person.id;
  return withPerson ? `${who}'s ${connection.client.name}` : connection.client.name;
}

export function Steps({ children }: { children: ReactNode }) {
  return <ol className="steps">{children}</ol>;
}

/** A numbered step of a page read top to bottom; a done step shows a check. */
export function Step({
  n,
  title,
  done = false,
  children,
}: {
  n: number;
  title: string;
  done?: boolean;
  children: ReactNode;
}) {
  return (
    <li
      className={done ? "step done" : "step"}
      aria-label={`Step ${n}: ${title}${done ? " (done)" : ""}`}
    >
      <span className="step-mark" aria-hidden="true">
        {done ? <Check size={13} strokeWidth={2.5} /> : n}
      </span>
      <div className="step-body">
        <h2 className="step-title">{title}</h2>
        {children}
      </div>
    </li>
  );
}
