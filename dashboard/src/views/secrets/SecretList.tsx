// The secrets of the organization (`namespace` null) or of one namespace, as a table: each one's
// labels, who wrote it and when, its reads over 30 days, its last read, and the runs and workflows
// that use it (archi/05, "Secrets"). A value is written in a dialog and never shown nor read
// back; a deletion is confirmed in a dialog that first says what it would stop, read from
// `GET …/secrets/{name}/usage`. Writing needs `secrets:write`, and a namespace's secret a grant
// that is not limited to labels.
import { PenLine, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";
import { runPath } from "../../app/runLinks";
import { Ago, Empty, LabelList, Loading, RunHandle, RunId, useNow } from "../../components/bits";
import { Sparkline } from "../../components/Charts";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import { HoverCard } from "../../components/HoverCard";
import { NamespaceName } from "../../components/NamespaceName";
import { TableScroll } from "../../components/TableScroll";
import { formatLabels } from "../../model/access";
import { formatCount } from "../../model/format";
import {
  labelsTaken,
  plural,
  READ_DAYS,
  readSeries,
  secretNameProblem,
  secretValueProblem,
  stopsNothing,
  totalReads,
} from "../../model/secrets";
import type { Secret, SecretUsage, WorkflowSecrets } from "../../model/types";
import { shortIdOf, versionLabel, workflowPath } from "../../model/workflows";
import { useAccess } from "../../state/access";
import { useDeleteSecret, usePutSecret, useSecrets, useSecretUsage } from "../../state/secrets";
import { useWorkflowLabels, useWorkflowScopes } from "../../state/workflows";
import { Field, LabelsInput, PrincipalName, useLabels, useMemberNames } from "../org/bits";

/** Who may write here: `secrets:write`, and for a namespace's secret a grant without labels. */
export function useCanWriteSecrets(namespace: string | null): boolean {
  const access = useAccess();
  return access.can("secrets:write") && (namespace === null || !access.limited);
}

export function SecretList({
  namespace,
}: {
  /** `null`: the organization's secrets. */
  namespace: string | null;
}) {
  const list = useSecrets(namespace);
  const write = useCanWriteSecrets(namespace);
  const names = useMemberNames();
  const now = useNow(60_000);
  const [writing, setWriting] = useState<{ existing: Secret | null } | null>(null);
  const [deleting, setDeleting] = useState<Secret | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const org = namespace === null;

  return (
    <div className="org-section secrets">
      {write && (
        <div className="section-bar">
          <button
            type="button"
            className="btn"
            onClick={() => {
              setNotice(null);
              setWriting({ existing: null });
            }}
          >
            <Plus size={12} /> New secret
          </button>
        </div>
      )}
      {notice && (
        <div className="notice" role="status">
          {notice}
        </div>
      )}
      <FormDialog open={writing !== null} onOpenChange={(open) => !open && setWriting(null)}>
        {writing && (
          <WriteSecret
            namespace={namespace}
            existing={writing.existing}
            taken={(list.data ?? []).map((s) => s.name)}
            onDone={(secret, created) => {
              setWriting(null);
              setNotice(
                created
                  ? `The secret ${secret.reference} was created. Its value is not shown again.`
                  : `The value of ${secret.reference} was replaced: its parked calls are sent again.`,
              );
            }}
          />
        )}
      </FormDialog>
      <FormDialog open={deleting !== null} onOpenChange={(open) => !open && setDeleting(null)}>
        {deleting && (
          <DeleteSecret
            namespace={namespace}
            secret={deleting}
            onDone={() => {
              setNotice(`The secret ${deleting.reference} was deleted.`);
              setDeleting(null);
            }}
          />
        )}
      </FormDialog>
      {list.isPending ? (
        <Loading />
      ) : list.isError ? (
        <ErrorNote error={list.error} title="The secrets could not be read" />
      ) : list.data.length === 0 ? (
        <Empty>
          {org
            ? "No secret yet: the runs use none of the organization's."
            : "No secret of this end user yet."}
        </Empty>
      ) : (
        <TableScroll>
          <table className="grid secrets-table">
            <thead>
              <tr>
                <th>Name</th>
                {org && <th>Labels</th>}
                <th>Written</th>
                <th>Reads, {READ_DAYS} days</th>
                <th>Last read</th>
                <th>Used by</th>
                {write && <th className="right">Actions</th>}
              </tr>
            </thead>
            <tbody>
              {list.data.map((s) => (
                <tr key={s.reference}>
                  <td>
                    <span className="two-lines">
                      <span className="key-name">{s.name}</span>
                      <span className="mono">{s.reference}</span>
                    </span>
                  </td>
                  {org && (
                    <td className="wrap">
                      <LabelList labels={s.labels ?? {}} />
                    </td>
                  )}
                  <td className="dim">
                    <span
                      className="two-lines"
                      title={`Created by ${s.createdBy} on ${new Date(Date.parse(s.createdAt)).toUTCString()}`}
                    >
                      <PrincipalName text={s.updatedBy} names={names} />
                      <Ago ms={Date.parse(s.updatedAt)} />
                    </span>
                  </td>
                  <td>
                    <span className="secret-reads">
                      <Sparkline
                        series={readSeries(s.reads, now)}
                        label={`Reads of ${s.reference} per day`}
                        width={90}
                        height={18}
                        tone="neutral"
                      />
                      <span className="num">{formatCount(totalReads(s.reads))}</span>
                    </span>
                  </td>
                  <td className="dim">
                    <LastRead secret={s} org={org} />
                  </td>
                  <td>
                    <UsedBy namespace={namespace} secret={s} />
                  </td>
                  {write && (
                    <td className="right">
                      <span className="row-actions">
                        <button
                          type="button"
                          className="icon-btn"
                          aria-label={`Write a new value of ${s.reference}`}
                          onClick={() => {
                            setNotice(null);
                            setWriting({ existing: s });
                          }}
                          title="Write a new value"
                        >
                          <PenLine size={13} aria-hidden="true" />
                        </button>
                        <button
                          type="button"
                          className="icon-btn danger"
                          aria-label={`Delete ${s.reference}`}
                          onClick={() => {
                            setNotice(null);
                            setDeleting(s);
                          }}
                          title="Delete…"
                        >
                          <Trash2 size={13} aria-hidden="true" />
                        </button>
                      </span>
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </TableScroll>
      )}
    </div>
  );
}

function LastRead({ secret, org }: { secret: Secret; org: boolean }) {
  const last = secret.lastRead;
  if (!last) return <span className="muted">never</span>;
  // An action set's registration reads it to list the actions of the server it is used for,
  // before any run exists: no run to link to.
  if (!last.run)
    return (
      <span className="two-lines">
        <Ago ms={Date.parse(last.at)} />
        <span title="Read to list the actions of the MCP server it is used for">
          at an action set's registration
        </span>
      </span>
    );
  return (
    <span className="two-lines">
      <Ago ms={Date.parse(last.at)} />
      <span className="wraps">
        {"by "}
        <Link to={runPath(last.runHandle ?? last.run)} title="The run that read it last">
          {last.runHandle ? (
            <RunHandle handle={last.runHandle} id={last.run} namespace={false} copy={false} />
          ) : (
            <RunId id={last.run} />
          )}
        </Link>
        {org && last.namespace && (
          <>
            {" · "}
            <NamespaceName name={last.namespace} />
          </>
        )}
      </span>
    </span>
  );
}

/**
 * How a workflow that uses a secret shows: its name, else what it shows as now; a namespace's
 * workflow that shares its label with another of the list (a fork keeps its source's name) says
 * its namespace too.
 */
function useWorkflowName(): (w: WorkflowSecrets, all?: readonly WorkflowSecrets[]) => string {
  const labelOf = useWorkflowLabels();
  const scopeOf = useWorkflowScopes();
  const label = (w: WorkflowSecrets) => w.name || labelOf(w.workflow) || shortIdOf(w.workflow);
  return (w, all = []) => {
    const namespace = scopeOf(w.workflow);
    const shared = all.some((o) => o.workflow !== w.workflow && label(o) === label(w));
    return shared && namespace ? `${label(w)} · ${namespace}` : label(w);
  };
}

/** The runs and workflows that use the secret now. */
function UsedBy({ namespace, secret }: { namespace: string | null; secret: Secret }) {
  const usage = useSecretUsage(namespace, secret.name);
  const access = useAccess();
  const nameOf = useWorkflowName();
  if (usage.isPending) return <span className="muted">…</span>;
  if (usage.isError) return <span className="muted">unknown</span>;
  const u = usage.data;
  if (stopsNothing(u)) return <span className="muted">nothing active</span>;
  return (
    <span className="used-by">
      {u.activeTotal > 0 && (
        <HoverCard
          label={`The active runs that use ${secret.reference}`}
          trigger={
            <button type="button" className="link-btn">
              {plural(u.activeTotal, "active run")}
              {u.waitingTotal > 0 ? `, ${u.waitingTotal} waiting` : ""}
            </button>
          }
        >
          <p className="card-title">{plural(u.activeTotal, "active run")} use it</p>
          <RunList usage={u} />
        </HoverCard>
      )}
      {u.workflows.map((w) =>
        access.can("runs:read") ? (
          <Link
            key={w.workflow}
            className="tag"
            to={workflowPath(w.workflow, w.version)}
            title={`${versionLabel(nameOf(w, u.workflows), w.version)}, its latest version, uses it for ${w.servers.join(", ")}`}
          >
            {nameOf(w, u.workflows)}
          </Link>
        ) : (
          <span key={w.workflow} className="tag" title={`For ${w.servers.join(", ")}`}>
            {nameOf(w, u.workflows)}
          </span>
        ),
      )}
    </span>
  );
}

function RunList({ usage, max = 12 }: { usage: SecretUsage; max?: number }) {
  const waiting = new Set(usage.waitingRuns);
  const shown = usage.activeRuns.slice(0, max);
  return (
    <ul className="plain-list secret-runs">
      {shown.map((id) => (
        <li key={id}>
          <Link to={`/runs/${id}`}>
            <RunId id={id} />
          </Link>
          {waiting.has(id) && <span className="muted"> · waits on a call it serves</span>}
        </li>
      ))}
      {usage.activeTotal > shown.length && (
        <li className="muted">and {formatCount(usage.activeTotal - shown.length)} more</li>
      )}
    </ul>
  );
}

/** Writes a new secret, or a new value of one: the value is typed, sent, and never read back. */
function WriteSecret({
  namespace,
  existing,
  taken,
  onDone,
}: {
  namespace: string | null;
  existing: Secret | null;
  /** The names in use: a new secret of one of them replaces it. */
  taken: readonly string[];
  onDone: (secret: Secret, created: boolean) => void;
}) {
  const access = useAccess();
  const org = namespace === null;
  const grantLabels = access.identity.grant?.allowedLabels ?? {};
  const [name, setName] = useState(existing?.name ?? "");
  const [value, setValue] = useState("");
  const [touched, setTouched] = useState(false);
  const labels = useLabels(existing?.labels ?? (access.limited ? grantLabels : undefined));
  const nameError = existing ? null : secretNameProblem(name);
  const valueError = secretValueProblem(value);
  const replaces = !existing && taken.includes(name);
  const save = usePutSecret();
  const ready = !nameError && !valueError && (!org || labels.labels !== null);
  const reference = `${org ? "org" : "ns"}/${name || "<name>"}`;
  return (
    <DialogForm
      title={existing ? `New value of ${existing.reference}` : "New secret"}
      description={
        existing ? (
          <>
            The value replaces the current one at once. The calls parked because it was missing or
            refused, in the runs that use it, are sent again with the same idempotency key.
          </>
        ) : org ? (
          <>
            A value the runs of every end user may use, as <code>{reference}</code>. Only Rewind's
            dispatcher reads it, for the runs that use it.
          </>
        ) : (
          <>
            This end user's own value, used as <code>{reference}</code> by the runs of this
            namespace only.
          </>
        )
      }
      submit={existing || replaces ? "Replace the value" : "Create the secret"}
      pending={save.pending}
      pendingLabel="Saving…"
      disabled={!ready}
      error={save.error}
      errorTitle="The secret was not saved"
      onSubmit={() => {
        setTouched(true);
        if (!ready) return;
        const body = {
          value,
          ...(org ? { labels: labelsTaken(labels.labels ?? {}, grantLabels) } : {}),
        };
        void save.run({ namespace, name, body }, (secret) => {
          setValue("");
          onDone(secret, !existing && !replaces);
        });
      }}
    >
      {!existing && (
        <Field
          label="Name"
          hint={
            replaces
              ? "A secret of this name exists: saving replaces its value."
              : "Lowercase letters, digits, - and _, starting with a letter: github, zendesk."
          }
          error={touched || name.length > 0 ? nameError : null}
        >
          {(props) => (
            <input
              {...props}
              className="input mono"
              placeholder="github"
              value={name}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setName(e.target.value)}
            />
          )}
        </Field>
      )}
      <Field
        label="Value"
        hint="Written, never shown again: no one reads it back, not even you. At most 16 KiB."
        error={touched ? valueError : null}
      >
        {(props) => (
          <input
            {...props}
            type="password"
            className="input mono"
            value={value}
            spellCheck={false}
            autoComplete="new-password"
            onChange={(e) => setValue(e.target.value)}
          />
        )}
      </Field>
      {org && (
        <LabelsInput
          label="Labels"
          value={labels.text}
          onChange={labels.setText}
          error={labels.error}
          placeholder="env:prod"
          hint={
            access.limited
              ? `Your grant is limited to ${formatLabels(grantLabels)}: the secret takes them.`
              : "Optional. Only the keys and grants limited to these labels see and use it, with those without labels."
          }
        />
      )}
    </DialogForm>
  );
}

/** Deletes a secret, once the dialog has said what that would stop. */
function DeleteSecret({
  namespace,
  secret,
  onDone,
}: {
  namespace: string | null;
  secret: Secret;
  onDone: () => void;
}) {
  const usage = useSecretUsage(namespace, secret.name);
  const remove = useDeleteSecret();
  return (
    <DialogForm
      title={`Delete ${secret.reference}?`}
      submit="Delete"
      pending={remove.pending}
      pendingLabel="Deleting…"
      disabled={usage.isPending}
      danger
      error={remove.error}
      errorTitle="It was not deleted"
      onSubmit={() => void remove.run({ namespace, name: secret.name }, onDone)}
    >
      {usage.isPending ? (
        <Loading>Reading what uses it…</Loading>
      ) : usage.isError ? (
        <ErrorNote error={usage.error} title="What uses it could not be read" />
      ) : (
        <Impact namespace={namespace} usage={usage.data} />
      )}
    </DialogForm>
  );
}

/** What deleting the secret stops, from its usage. */
function Impact({ namespace, usage }: { namespace: string | null; usage: SecretUsage }) {
  const nameOf = useWorkflowName();
  const reads = totalReads(usage.secret.reads);
  const last = usage.secret.lastRead;
  return (
    <div className="secret-impact" data-testid="impact">
      {stopsNothing(usage) ? (
        <p className="dialog-text">No active run and no workflow uses it.</p>
      ) : (
        <>
          {usage.activeTotal > 0 && (
            <div>
              <p className="dialog-text">
                <strong>{plural(usage.activeTotal, "active run")}</strong> use it: their next call
                to the server it serves is parked until a secret of this name is put again.
                {usage.waitingTotal > 0 && (
                  <>
                    {" "}
                    <strong>{usage.waitingTotal}</strong> of them wait on such a call now.
                  </>
                )}
              </p>
              <RunList usage={usage} max={8} />
            </div>
          )}
          {usage.workflows.length > 0 && (
            <div>
              <p className="dialog-text">
                The latest version of <strong>{plural(usage.workflows.length, "workflow")}</strong>{" "}
                uses it:{" "}
                {namespace === null
                  ? "their runs start, and park their calls to the server it serves."
                  : "their runs in this namespace are refused at start (secret-required)."}
              </p>
              <ul className="plain-list">
                {usage.workflows.map((w) => (
                  <li key={w.workflow}>
                    <span className="key-name">{nameOf(w, usage.workflows)}</span>{" "}
                    <span className="num muted" title={w.digest}>
                      v{w.version}
                    </span>{" "}
                    for {w.servers.join(", ")}
                  </li>
                ))}
              </ul>
            </div>
          )}
        </>
      )}
      <p className="dialog-text muted">
        Read {plural(reads, "time")} in {READ_DAYS} days
        {last ? (
          <>
            , last <Ago ms={Date.parse(last.at)} />
          </>
        ) : null}
        . A deleted secret cannot be restored: its value is gone.
      </p>
    </div>
  );
}
