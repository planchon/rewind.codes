// Connect Claude (archi/01, "Claude as the harness"): a page read top to bottom, from a toolset to
// a first message, beside the good practices and the person's connections. The fourth step asks
// every 2 s, while the page is open, whether a connection arrived or was used since it opened.
import {
  ArrowUpRight,
  ChevronRight,
  CircleAlert,
  Info,
  Plug,
  TriangleExclamationPoint,
} from "lucide-react";
import { type ReactNode, useState } from "react";
import { Link } from "react-router";
import { useServer } from "../../app/Gate";
import { Ago, Empty, Loading } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import { PageHeader } from "../../components/PageHeader";
import {
  firstMessage,
  hostTools,
  type McpInfo,
  mcpOf,
  namespaceProblem,
  toolsetTitle,
} from "../../model/connections";
import { shortDigest } from "../../model/format";
import type { Connection, ToolsetSummary } from "../../model/types";
import { useAccess } from "../../state/access";
import { useArrivingConnections } from "../../state/connections";
import { useSession } from "../../state/session";
import { useToolsets } from "../../state/toolsets";
import { Field } from "../org/bits";
import { ClientName, CopyBlock, OrganizationList, Step, Steps } from "./bits";

const DOCS = "https://docs.rewind.codes";

function DocsLink({ path, children }: { path: string; children: ReactNode }) {
  return (
    <a href={`${DOCS}${path}`} target="_blank" rel="noreferrer" className="ext-link">
      {children}
      <ArrowUpRight size={12} aria-hidden="true" />
    </a>
  );
}

export function ConnectView() {
  const access = useAccess();
  const server = useServer();
  const { phase } = useSession();
  const mcp = mcpOf(server);
  const member = phase.kind === "member";
  const readsToolsets = access.can("toolsets:read");
  const toolsets = useToolsets(readsToolsets);
  // Connections are a person's, made through Rewind's OAuth: asked for while the page is open. A
  // step turns green when one arrives, compared with what they were when the page opened.
  const watching = member && mcp.oauth && mcp.mcpUrl !== null;
  const { connections, arrived, ready } = useArrivingConnections(member, watching);

  const list = toolsets.data ?? [];
  const organization = access.organization?.name ?? access.identity.tenant ?? "your organization";

  return (
    <div className="page">
      <PageHeader title="Connect Claude" />
      <p className="page-intro">
        Use Rewind from Claude: claude.ai, Claude Desktop, the mobile apps or Claude Code. Claude
        writes the scripts, checks and tests them, runs them and follows their runs, with the tools
        of your toolsets.
      </p>
      <div className="connect-grid">
        <Steps>
          <Step n={1} title="Have a toolset" done={list.length > 0}>
            <ToolsetStep
              readable={readsToolsets}
              pending={toolsets.isPending}
              error={toolsets.isError ? toolsets.error : null}
              toolsets={list}
              organization={organization}
            />
          </Step>
          <Step n={2} title="Add the connector">
            <ConnectorStep mcp={mcp} />
          </Step>
          <Step n={3} title="Authorize Claude">
            <AuthorizeStep mcp={mcp} />
          </Step>
          <Step n={4} title="Check the connection" done={arrived !== null}>
            <CheckStep
              mcp={mcp}
              member={member}
              arrived={arrived}
              count={connections.data?.length ?? 0}
              error={connections.isError ? connections.error : null}
              ready={ready}
            />
          </Step>
          <Step n={5} title="Try it">
            <TryStep organization={organization} toolsets={list} />
          </Step>
        </Steps>
        <aside className="connect-side" aria-label="Good practices and your connections">
          <Practices />
          <YourConnections
            member={member}
            pending={connections.isPending}
            connections={connections.data ?? []}
          />
        </aside>
      </div>
    </div>
  );
}

function ToolsetStep({
  readable,
  pending,
  error,
  toolsets,
  organization,
}: {
  readable: boolean;
  pending: boolean;
  error: unknown;
  toolsets: readonly ToolsetSummary[];
  organization: string;
}) {
  if (!readable) {
    return (
      <p className="step-text">
        Claude writes scripts against a toolset of {organization}. Your role cannot list them (
        <code>toolsets:read</code>): ask an owner which one to use.
      </p>
    );
  }
  if (pending) return <Loading />;
  if (error) return <ErrorNote error={error} />;
  if (toolsets.length === 0) {
    return (
      <div className="stack">
        <p className="step-text">
          Claude writes scripts against a toolset: the MCP servers and host tools they may call.{" "}
          {organization} has none yet. Register one with the SDK (
          <code>rewind.toolsets.create</code>) or the CLI (
          <code>rewind toolsets add toolset.json</code>).
        </p>
        <p className="step-text">
          <DocsLink path="/sdk/toolsets">Create a toolset</DocsLink>
        </p>
      </div>
    );
  }
  return (
    <div className="stack">
      <p className="step-text">
        Claude writes scripts against one of the toolsets of {organization}, and finds them with{" "}
        <code>list_toolsets</code>.
      </p>
      <ul className="toolset-list">
        {toolsets.map((t) => {
          const host = hostTools(t);
          return (
            <li key={t.id}>
              <div className="toolset-line">
                <strong>{toolsetTitle(t)}</strong>
                <span className="digest" title={t.id}>
                  {shortDigest(t.id)}
                </span>
                <span className="muted">
                  {t.tools.length} tool{t.tools.length === 1 ? "" : "s"}
                  {t.workflows && t.workflows.length > 0
                    ? ` · workflows ${t.workflows.join(", ")}`
                    : ""}
                </span>
              </div>
              {host.length > 0 && (
                <p className="toolset-warning">
                  <TriangleExclamationPoint size={12} aria-hidden="true" />
                  Host tools ({host.join(", ")}): Claude reaches them only while your worker runs.
                </p>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

type Client = "claude" | "team" | "code" | "other";

const CLIENTS: { id: Client; title: string }[] = [
  { id: "claude", title: "claude.ai (Pro, Max)" },
  { id: "team", title: "Team, Enterprise" },
  { id: "code", title: "Claude Code" },
  { id: "other", title: "Other MCP clients" },
];

function NoOAuth() {
  return (
    <p className="connect-note">
      <Info size={13} aria-hidden="true" />
      This server signs no one in for an MCP client: it needs its public URL and WorkOS. Its{" "}
      <code>/mcp</code> takes an API key or an end user's token as Bearer, from clients that send a
      header.
    </p>
  );
}

function ConnectorStep({ mcp }: { mcp: McpInfo }) {
  const [client, setClient] = useState<Client>("claude");
  const url = mcp.mcpUrl;
  if (url === null) {
    return (
      <p className="connect-note">
        <CircleAlert size={13} aria-hidden="true" />
        MCP is not enabled on this server: it has no address Claude can reach. Use Rewind's hosted
        server, or give yours its public URL.
      </p>
    );
  }
  return (
    <div className="stack">
      <p className="step-text">
        Rewind is an MCP server at this address, the same for every client:
      </p>
      <CopyBlock text={url} label="Copy the URL" />
      <div className="connect-tabs">
        <div className="tabs" role="tablist" aria-label="Clients">
          {CLIENTS.map((c) => (
            <button
              key={c.id}
              type="button"
              role="tab"
              id={`client-${c.id}`}
              aria-controls="client-panel"
              aria-selected={client === c.id}
              onClick={() => setClient(c.id)}
            >
              {c.title}
            </button>
          ))}
        </div>
        <div
          className="client-panel"
          role="tabpanel"
          id="client-panel"
          aria-labelledby={`client-${client}`}
        >
          {client === "claude" &&
            (mcp.oauth ? (
              <>
                <ol className="client-steps">
                  <li>
                    In Claude, open <strong>Customize › Connectors</strong>, then{" "}
                    <strong>Add › Add custom connector</strong>.
                  </li>
                  <li>
                    Name it <strong>Rewind</strong> and paste the URL.
                  </li>
                  <li>
                    Keep the settings marked <strong>Detected</strong> as they are,{" "}
                    <em>Use Claude's published identity</em> among them.
                  </li>
                  <li>
                    <strong>Add</strong>, then <strong>Connect</strong>: Claude opens Rewind's page
                    (step 3).
                  </li>
                </ol>
                <p className="muted">
                  The connector then follows you to Claude Desktop and the mobile apps.
                </p>
              </>
            ) : (
              <NoOAuth />
            ))}
          {client === "team" &&
            (mcp.oauth ? (
              <ol className="client-steps">
                <li>
                  An Owner of the Claude organization adds it once:{" "}
                  <strong>Organization settings › Connectors › Add › Custom › Web</strong>, named{" "}
                  <strong>Rewind</strong>, with the URL.
                </li>
                <li>
                  Each member then finds Rewind in <strong>Customize › Connectors</strong> and{" "}
                  <strong>Connect</strong>s with their own Rewind account: everyone gets their own
                  connection, within their own role.
                </li>
              </ol>
            ) : (
              <NoOAuth />
            ))}
          {client === "code" &&
            (mcp.oauth ? (
              <div className="stack">
                <CopyBlock
                  text={`claude mcp add --transport http rewind ${url}`}
                  label="Copy the command"
                />
                <p className="step-text">
                  Then run <code>/mcp</code> in Claude Code, pick <strong>rewind</strong> and sign
                  in: the browser opens Rewind's page. With <code>--scope project</code>, the
                  command writes the server to the repository's <code>.mcp.json</code>, for the
                  team; each person still signs in with their own account.
                </p>
              </div>
            ) : (
              <div className="stack">
                <CopyBlock
                  text={`claude mcp add --transport http rewind ${url} --header "Authorization: Bearer $REWIND_API_KEY"`}
                  label="Copy the command"
                />
                <NoOAuth />
              </div>
            ))}
          {client === "other" && (
            <div className="stack">
              <p className="step-text">
                Add a remote MCP server (Streamable HTTP) with the same URL. A client that signs in
                with OAuth finds Rewind's sign-in from the URL and registers itself; you authorize
                it as you would Claude.
              </p>
              <p className="step-text">
                Without a browser (CI, the Claude API's MCP connector), send an API key or an end
                user's token as <code>Authorization: Bearer</code>.{" "}
                <DocsLink path="/claude#without-a-browser">How</DocsLink>
              </p>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function AuthorizeStep({ mcp }: { mcp: McpInfo }) {
  if (mcp.mcpUrl === null || !mcp.oauth) {
    return (
      <p className="step-text muted">
        Nothing to authorize here: the key or token the client sends decides what it may do.
      </p>
    );
  }
  return (
    <div className="stack">
      <p className="step-text">
        Claude opens Rewind's page in your browser; sign in if asked. Tick the organizations Claude
        may open, and give each an access:
      </p>
      <dl className="access-list">
        <dt>Read</dt>
        <dd>runs, namespaces, toolsets and workflows.</dd>
        <dt>Run</dt>
        <dd>also check, test and start scripts, signal and cancel runs.</dd>
      </dl>
      <p className="step-text">
        Claude never gets more than your own role gives, and never administers anything. The runs it
        starts carry the label <code>via: claude</code>, unless you untick it.
      </p>
    </div>
  );
}

function CheckStep({
  mcp,
  member,
  arrived,
  count,
  error,
  ready,
}: {
  mcp: McpInfo;
  member: boolean;
  arrived: Connection | null;
  count: number;
  error: unknown;
  ready: boolean;
}) {
  if (!member) {
    return (
      <p className="step-text muted">
        Connections belong to people: sign in to the dashboard to see yours arrive.
      </p>
    );
  }
  if (mcp.mcpUrl === null || !mcp.oauth) {
    return (
      <p className="step-text muted">
        This server makes no connection: call a tool from the client to check its key.
      </p>
    );
  }
  if (error) return <ErrorNote error={error} />;
  if (arrived) {
    const used = arrived.lastUsedAt ?? arrived.createdAt;
    return (
      <output className="connect-status good">
        <span className="live-dot" aria-hidden="true" />
        <span>
          <strong>Connected</strong>: {arrived.client.name}, {arrived.lastUsedAt ? "used" : "made"}{" "}
          <Ago ms={Date.parse(used)} />.
        </span>
      </output>
    );
  }
  return (
    <output className="connect-status">
      <span className="spinner" aria-hidden="true" />
      <span>
        {ready ? "Waiting for Claude to connect…" : "Reading your connections…"}
        {ready && count > 0 && (
          <span className="muted">
            {" "}
            You have {count} connection{count === 1 ? "" : "s"}: this turns green when one is used.
          </span>
        )}
      </span>
    </output>
  );
}

function TryStep({
  organization,
  toolsets,
}: {
  organization: string;
  toolsets: readonly ToolsetSummary[];
}) {
  const [toolset, setToolset] = useState<string | null>(null);
  const [namespace, setNamespace] = useState("demo");
  const chosen = toolset ?? toolsets[0]?.id ?? null;
  const problem = namespaceProblem(namespace);
  const text = firstMessage({ organization, toolset: chosen, namespace });
  return (
    <div className="stack">
      <p className="step-text">
        Send Claude a first message that names the organization, a toolset and a namespace (an end
        user, created at its first run), so that it asks nothing before it starts:
      </p>
      <div className="form-row">
        {toolsets.length > 0 && (
          <Field label="Toolset" hint="Its id, as list_toolsets gives it.">
            {(props) => (
              <select
                {...props}
                className="input"
                value={chosen ?? ""}
                onChange={(e) => setToolset(e.target.value)}
              >
                {toolsets.map((t) => (
                  <option key={t.id} value={t.id}>
                    {toolsetTitle(t)} · {shortDigest(t.id)}
                  </option>
                ))}
              </select>
            )}
          </Field>
        )}
        <Field label="Namespace" hint="The end user the runs act for." error={problem}>
          {(props) => (
            <input
              {...props}
              className="input mono"
              value={namespace}
              spellCheck={false}
              onChange={(e) => setNamespace(e.target.value)}
            />
          )}
        </Field>
      </div>
      <CopyBlock text={text} label="Copy the message" wrap />
    </div>
  );
}

function Practices() {
  return (
    <section className="panel" aria-labelledby="practices">
      <div className="panel-head">
        <h2 id="practices">Good practices</h2>
      </div>
      <ul className="practices panel-body">
        <li>
          <strong>One connection per person.</strong> Everyone connects with their own Rewind
          account, never a shared one.
        </li>
        <li>
          <strong>Name the organization, namespace and toolset</strong> in your message: nothing is
          pinned at connection.
        </li>
        <li>
          <strong>The least access that works.</strong> Read to look at runs; Run only where Claude
          should start them.
        </li>
        <li>
          <strong>Keep Claude's confirmation</strong> on <code>run_script</code>,{" "}
          <code>retry_run</code>, <code>signal_run</code> and <code>cancel_run</code>. The read-only
          tools may be "Always allow".
        </li>
        <li>
          <strong>Secrets stay in Rewind.</strong> Their values are written in{" "}
          <Link to="/secrets">Secrets</Link>, never in the chat: Claude attaches a secret by its
          name, and a script never sees it.
        </li>
        <li>
          <strong>Revoke</strong> a connection you no longer use from{" "}
          <Link to="/account/connections">Account › Connections</Link>.
        </li>
      </ul>
    </section>
  );
}

function YourConnections({
  member,
  pending,
  connections,
}: {
  member: boolean;
  pending: boolean;
  connections: readonly Connection[];
}) {
  return (
    <section className="panel" aria-labelledby="your-connections">
      <div className="panel-head">
        <h2 id="your-connections">
          <Plug size={14} aria-hidden="true" /> Your connections
        </h2>
        {member && (
          <span className="end">
            <Link to="/account/connections">
              Manage <ChevronRight size={12} aria-hidden="true" />
            </Link>
          </span>
        )}
      </div>
      <div className="panel-body">
        {!member ? (
          <p className="muted">Connections belong to people: an API key has none.</p>
        ) : pending ? (
          <Loading />
        ) : connections.length === 0 ? (
          <Empty>None yet.</Empty>
        ) : (
          <ul className="conn-summary">
            {connections.map((c) => (
              <li key={c.id}>
                <div className="conn-summary-head">
                  <ClientName client={c.client} />
                  <span className="muted nowrap">
                    {c.lastUsedAt ? (
                      <>
                        used <Ago ms={Date.parse(c.lastUsedAt)} />
                      </>
                    ) : (
                      "never used"
                    )}
                  </span>
                </div>
                <OrganizationList organizations={c.organizations} />
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}
