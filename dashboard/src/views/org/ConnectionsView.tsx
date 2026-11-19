// Organization › Connections: the connections of people that open the organization (MCP clients
// acting for a member, Claude above all), shown with `members:read`. Each has the access its
// person chose there, never more than their own role. Whoever holds `members:manage` removes the
// organization from a connection, in a dialog that asks once more; the person's other
// organizations stay open to it.
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import {
  CONNECTIONS,
  type Connection,
  ORG_CONNECTIONS,
  removeOrganizationConnection,
  useOrganizationConnections,
} from "../../api/connections";
import { useMembers } from "../../api/organization";
import { useAccess } from "../../app/access";
import { useApi } from "../../app/session";
import { Ago, Empty, LabelList, Loading } from "../../components/bits";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { ErrorNote } from "../../components/ErrorNote";
import { AccessTag, ClientName, connectionName } from "../claude/bits";
import { Person } from "./bits";
import { SectionGate } from "./OrganizationView";

export function ConnectionsView() {
  return (
    <SectionGate path="connections">
      <OrganizationConnections />
    </SectionGate>
  );
}

function Who({ connection, you }: { connection: Connection; you: boolean }) {
  const members = useMembers();
  const member = members.data?.find((m) => m.user.id === connection.person.id);
  if (member) return <Person member={member} you={you} />;
  const { name, email, id } = connection.person;
  return (
    <span className="person">
      <span className="person-text">
        <span className="person-name">
          {name ?? email ?? id}
          {you && <span className="tag">you</span>}
        </span>
        {name && email && <span className="muted">{email}</span>}
      </span>
    </span>
  );
}

function OrganizationConnections() {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const connections = useOrganizationConnections();
  const manage = access.can("members:manage");
  const [removing, setRemoving] = useState<Connection | null>(null);
  const remove = useMutation({
    mutationFn: (id: string) => removeOrganizationConnection(api, id),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ORG_CONNECTIONS });
      await queryClient.invalidateQueries({ queryKey: CONNECTIONS });
      setRemoving(null);
    },
  });
  const org = access.organization?.name ?? "the organization";

  return (
    <section className="org-section" aria-label="Connections">
      <p className="section-intro dim">
        The MCP clients members of {org} let act for them, Claude above all: each with the access
        its person chose here, never more than their own role.
        {manage
          ? " Removing the organization from a connection leaves its person's other organizations to it."
          : ""}
      </p>
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(open) => {
          if (!open) setRemoving(null);
        }}
        title={`Remove ${org} from ${removing ? connectionName(removing, true) : "this connection"}?`}
        description={
          <>
            This connection stops reaching {org} at its next call. The other organizations its
            person opened to it stay open; they may open {org} to it again from their account.
          </>
        }
        confirm="Remove"
        pendingLabel="Removing…"
        pending={remove.isPending}
        error={remove.error}
        onConfirm={() => {
          if (removing) remove.mutate(removing.id);
        }}
      />
      {connections.isPending ? (
        <Loading />
      ) : connections.error ? (
        <ErrorNote error={connections.error} />
      ) : connections.data.length === 0 ? (
        <Empty>No connection opens {org}.</Empty>
      ) : (
        <div className="table-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Person</th>
                <th>Client</th>
                <th>Access</th>
                <th>Labels</th>
                <th>Last used</th>
                {manage && <th className="right">Actions</th>}
              </tr>
            </thead>
            <tbody>
              {connections.data.map((c) => {
                const here = c.organizations[0];
                return (
                  <tr key={c.id}>
                    <td>
                      <Who connection={c} you={c.person.id === access.user?.id} />
                    </td>
                    <td>
                      <ClientName client={c.client} />
                    </td>
                    <td>{here ? <AccessTag access={here.access} /> : "–"}</td>
                    <td className="wrap">
                      <LabelList labels={here?.allowedLabels ?? {}} />
                    </td>
                    <td className="dim">
                      {c.lastUsedAt ? <Ago ms={Date.parse(c.lastUsedAt)} /> : "never"}
                    </td>
                    {manage && (
                      <td className="right">
                        <button
                          type="button"
                          className="btn small"
                          onClick={() => {
                            remove.reset();
                            setRemoving(c);
                          }}
                        >
                          Remove from organization
                        </button>
                      </td>
                    )}
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
