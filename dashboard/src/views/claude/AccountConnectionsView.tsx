// Account › Connections: the MCP clients the person let act for them, Claude above all. Each
// opens the organizations they ticked, with an access and labels; the person changes them in a
// dialog (`PATCH /v1/connections/{id}`), without reconnecting the client, or revokes the
// connection (`DELETE`) in a dialog that asks once more, which the client loses at once.
import { Plug } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";
import { Ago, Empty, Loading } from "../../components/bits";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import { PageHeader } from "../../components/PageHeader";
import { formatLabels, parseLabels } from "../../model/access";
import { ACCESS_INFO, ACCESSES } from "../../model/connections";
import type { Connection, ConnectionAccess, Labels } from "../../model/types";
import { useAccess } from "../../state/access";
import { useConnections, useRevokeConnection, useUpdateConnection } from "../../state/connections";
import { useSession } from "../../state/session";
import { ClientName, OrganizationList } from "./bits";

export function AccountConnectionsView() {
  const { phase } = useSession();
  return (
    <div className="page">
      <PageHeader crumbs="Account" title="Connections" />
      <div className="page-body org-body">
        {phase.kind === "member" ? (
          <Connections />
        ) : (
          <div className="no-permission" role="alert">
            <h1>Connections: a person's</h1>
            <p>
              A connection lets an MCP client act for a person signed in to Rewind. An API key has
              none: it is itself what a client sends.
            </p>
          </div>
        )}
      </div>
    </div>
  );
}

function Connections() {
  const connections = useConnections();
  const [editing, setEditing] = useState(false);
  const [target, setTarget] = useState<Connection | null>(null);
  const [revoking, setRevoking] = useState<Connection | null>(null);
  // Gone at once; a refusal brings it back, and the dialog says why.
  const revoke = useRevokeConnection();

  return (
    <section className="org-section" aria-label="Connections">
      <div className="section-bar">
        <p className="section-intro dim">
          The MCP clients you let act for you, Claude above all. Each opens the organizations you
          ticked, with an access, never more than your own role there. A change applies at the
          client's next call, without reconnecting it; a revoked connection stops at once.
        </p>
        <Link to="/connect" className="btn">
          <Plug size={12} /> Connect Claude
        </Link>
      </div>
      <FormDialog open={editing} onOpenChange={setEditing}>
        {target && <EditConnection connection={target} onDone={() => setEditing(false)} />}
      </FormDialog>
      <ConfirmDialog
        open={revoking !== null}
        onOpenChange={(open) => {
          if (!open) setRevoking(null);
        }}
        title={`Revoke ${revoking?.client.name ?? "this connection"}?`}
        description={
          <>
            {revoking?.client.name ?? "The client"} stops reaching Rewind at its next call, in every
            organization this connection opens. To use Rewind from it again, connect it again from
            the client.
          </>
        }
        confirm="Revoke"
        pendingLabel="Revoking…"
        pending={revoke.pending}
        error={revoke.error}
        onConfirm={() => {
          if (revoking) void revoke.run(revoking.id, () => setRevoking(null));
        }}
      />
      {connections.isPending ? (
        <Loading />
      ) : connections.isError ? (
        <ErrorNote error={connections.error} />
      ) : connections.data.length === 0 ? (
        <Empty>
          No connection yet. <Link to="/connect">Connect Claude</Link> to use Rewind from it.
        </Empty>
      ) : (
        <div className="table-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Client</th>
                <th>Organizations</th>
                <th>Created</th>
                <th>Last used</th>
                <th className="right">Actions</th>
              </tr>
            </thead>
            <tbody>
              {connections.data.map((c) => (
                <tr key={c.id}>
                  <td>
                    <ClientName client={c.client} />
                  </td>
                  <td className="wrap">
                    <OrganizationList organizations={c.organizations} />
                  </td>
                  <td className="dim">
                    <Ago ms={Date.parse(c.createdAt)} />
                  </td>
                  <td className="dim">
                    {c.lastUsedAt ? <Ago ms={Date.parse(c.lastUsedAt)} /> : "never"}
                  </td>
                  <td className="right">
                    <span className="row-actions">
                      <button
                        type="button"
                        className="btn small"
                        aria-label={`Edit ${c.client.name}`}
                        onClick={() => {
                          setTarget(c);
                          setEditing(true);
                        }}
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        className="btn small"
                        onClick={() => {
                          revoke.reset();
                          setRevoking(c);
                        }}
                      >
                        Revoke
                      </button>
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

interface Row {
  id: string;
  name: string;
  checked: boolean;
  access: ConnectionAccess;
  labels: string;
}

/**
 * The organizations of the person, each ticked when the connection opens it. The rights the
 * person holds are known in the organization open in the dashboard only: there, an access their
 * role does not give is disabled; elsewhere the server says so.
 */
function rowsOf(
  connection: Connection,
  organizations: readonly { id: string; name: string }[],
): Row[] {
  const opened = new Map(connection.organizations.map((o) => [o.id, o]));
  const usual = connection.organizations[0]?.allowedLabels ?? { via: "claude" };
  const all = new Map<string, string>();
  for (const o of organizations) all.set(o.id, o.name);
  for (const o of connection.organizations) if (!all.has(o.id)) all.set(o.id, o.name ?? o.id);
  return [...all]
    .map(([id, name]) => {
      const o = opened.get(id);
      return {
        id,
        name,
        checked: o !== undefined,
        access: o?.access ?? "read",
        labels: formatLabels(o?.allowedLabels ?? usual),
      };
    })
    .sort((a, b) => a.name.localeCompare(b.name));
}

function EditConnection({ connection, onDone }: { connection: Connection; onDone: () => void }) {
  const access = useAccess();
  const [rows, setRows] = useState(() => rowsOf(connection, access.organizations));
  const currentId = access.organization?.id ?? null;
  const givable = (id: string, a: ConnectionAccess): boolean => {
    if (id !== currentId) return true;
    if (a === "read")
      return (["runs:read", "toolsets:read", "namespaces:read"] as const).every((p) =>
        access.can(p),
      );
    return givable(id, "read") && access.can("runs:start") && access.can("runs:control");
  };
  const parsed = rows.map((r) => ({ row: r, labels: parseLabels(r.labels) }));
  const invalid = parsed.some(({ row, labels }) => row.checked && "error" in labels);
  const ticked = rows.filter((r) => r.checked).length;
  // The connection shows what it opens at once; a refusal takes it back, and says why here.
  const save = useUpdateConnection();
  const send = () =>
    void save.run(
      {
        id: connection.id,
        body: {
          organizations: parsed
            .filter(({ row }) => row.checked)
            .map(({ row, labels }) => ({
              id: row.id,
              access: row.access,
              allowedLabels: "labels" in labels ? (labels.labels as Labels) : {},
            })),
        },
        names: new Map(rows.map((r) => [r.id, r.name])),
      },
      onDone,
    );
  const set = (id: string, next: Partial<Row>) =>
    setRows((current) => current.map((r) => (r.id === id ? { ...r, ...next } : r)));

  return (
    <DialogForm
      title={`What ${connection.client.name} opens`}
      description={
        <>
          Tick the organizations it may open, with an access each: <strong>Read</strong> (
          {ACCESS_INFO.read.gives}) or <strong>Run</strong> ({ACCESS_INFO.run.gives}). Labels limit
          it to the runs carrying them, and mark the runs it starts.
        </>
      }
      submit="Save"
      pending={save.pending}
      disabled={ticked === 0 || invalid}
      error={save.error}
      onSubmit={() => {
        if (ticked > 0 && !invalid) send();
      }}
    >
      <fieldset className="conn-edit">
        <legend className="sr-only">Organizations</legend>
        {parsed.map(({ row, labels }) => (
          <div key={row.id} className="conn-edit-row">
            <label className="consent-org-check">
              <input
                type="checkbox"
                checked={row.checked}
                onChange={(e) => set(row.id, { checked: e.target.checked })}
              />
              <span className="consent-org-name">{row.name}</span>
            </label>
            <select
              className="input"
              aria-label={`Access in ${row.name}`}
              value={row.access}
              disabled={!row.checked}
              onChange={(e) => set(row.id, { access: e.target.value as ConnectionAccess })}
            >
              {ACCESSES.map((a) => (
                <option key={a} value={a} disabled={!givable(row.id, a)}>
                  {ACCESS_INFO[a].title}
                </option>
              ))}
            </select>
            <input
              className="input mono"
              aria-label={`Labels in ${row.name}`}
              aria-invalid={row.checked && "error" in labels ? true : undefined}
              placeholder="via:claude"
              spellCheck={false}
              value={row.labels}
              disabled={!row.checked}
              onChange={(e) => set(row.id, { labels: e.target.value })}
            />
            {row.checked && "error" in labels && (
              <span className="field-hint tone-critical">{labels.error}</span>
            )}
          </div>
        ))}
      </fieldset>
      {ticked === 0 && (
        <p className="field-hint">Tick one organization at least, or revoke the connection.</p>
      )}
    </DialogForm>
  );
}
