// What a workflow's header does, for whoever holds `workflows:write` (archi/05, "Workflows"): name
// and describe it, promote a namespace's workflow to the organization, fork an organization's
// workflow into a namespace, pull into a fork its source's latest version. Each opens a dialog over
// the page; the workflow keeps its id, its versions and its runs' handles.
import { ArrowUpToLine, GitFork, GitPullRequestArrow, PenLine } from "lucide-react";
import { useState } from "react";
import { useNavigate } from "react-router";
import { useSettled } from "../../components/bits";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import { NamespaceName } from "../../components/NamespaceName";
import { formatCount } from "../../model/format";
import type { Workflow } from "../../model/types";
import { workflowPath } from "../../model/workflows";
import { useAccess } from "../../state/access";
import { ApiError } from "../../state/errors";
import { useNamespaceSuggestions } from "../../state/namespaces";
import {
  useForkWorkflow,
  usePatchWorkflow,
  usePromoteWorkflow,
  usePullWorkflow,
  useWorkflow,
} from "../../state/workflows";
import { Field } from "../org/bits";

/** A name's limits, as the API checks them (archi/04, "Workflow names"); reserved words aside. */
const NAME_MAX = 63;
const DESCRIPTION_MAX = 2_000;

function nameProblem(name: string): string | null {
  if (name === "") return null;
  if (name.length > NAME_MAX) return `At most ${NAME_MAX} characters.`;
  if (!/^[A-Za-z_$][\w$]*$/.test(name))
    return "An identifier: letters, digits, _ and $, not starting with a digit.";
  if (name.startsWith("wf_")) return "A name never starts with wf_, which ids start with.";
  return null;
}

/** The server's refusal of a name, said under its field. */
function nameRefusal(error: unknown): string | null {
  if (!(error instanceof ApiError)) return null;
  if (error.slug === "name-taken")
    return error.problem.detail || "Its scope has a workflow of that name.";
  if (error.slug === "invalid-workflow") return error.problem.detail || "Not a valid name.";
  return null;
}

function EditForm({ workflow, onDone }: { workflow: Workflow; onDone: () => void }) {
  const patch = usePatchWorkflow(workflow.id);
  const [name, setName] = useState(workflow.name ?? "");
  const [description, setDescription] = useState(workflow.description ?? "");
  const trimmed = name.trim();
  const problem = nameProblem(trimmed);
  const refused = nameRefusal(patch.error);
  return (
    <DialogForm
      title="Name and description"
      meta={<span className="mono">{workflow.shortId}</span>}
      submit="Save"
      pending={patch.pending}
      disabled={problem !== null || description.trim().length > DESCRIPTION_MAX}
      error={refused ? null : patch.error}
      onSubmit={() =>
        void patch.run({ name: trimmed || null, description: description.trim() || null }, onDone)
      }
    >
      <Field
        label="Name"
        error={problem ?? refused}
        hint="Unique in its scope; runs and harnesses start it by its name. Empty removes it."
      >
        {(props) => (
          <input
            {...props}
            className="input mono"
            value={name}
            maxLength={NAME_MAX + 1}
            onChange={(e) => {
              setName(e.target.value);
              if (patch.error) patch.reset();
            }}
            placeholder="triage"
            autoComplete="off"
            spellCheck={false}
          />
        )}
      </Field>
      <Field
        label="Description"
        hint="What it does, for whoever reads it. Empty shows the comment at the top of its code."
      >
        {(props) => (
          <textarea
            {...props}
            className="input"
            rows={4}
            value={description}
            maxLength={DESCRIPTION_MAX}
            onChange={(e) => setDescription(e.target.value)}
          />
        )}
      </Field>
    </DialogForm>
  );
}

function PromoteForm({
  workflow,
  label,
  onDone,
}: {
  workflow: Workflow;
  label: string;
  onDone: () => void;
}) {
  const promote = usePromoteWorkflow(workflow.id);
  return (
    <DialogForm
      title={`Promote ${label}?`}
      description={
        <p>
          It moves from the namespace{" "}
          {workflow.namespace ? <NamespaceName name={workflow.namespace} /> : null} to the
          organization, with its id, its versions and its suite: every namespace's runs may then
          execute it, and a namespace may fork it.
        </p>
      }
      submit="Promote"
      pendingLabel="Promoting…"
      pending={promote.pending}
      error={promote.error}
      onSubmit={() => void promote.run(undefined, onDone)}
    >
      {null}
    </DialogForm>
  );
}

/** The namespaces to fork into: what is typed narrows them, the most active first. */
function NamespacePicker({
  picked,
  onPick,
}: {
  picked: string | null;
  onPick: (name: string) => void;
}) {
  const [typed, setTyped] = useState("");
  const prefix = useSettled(typed.trim());
  const found = useNamespaceSuggestions(prefix, 8);
  const namespaces = found.data?.namespaces ?? [];
  return (
    <div className="field col">
      <span className="field-label">Namespace</span>
      <div className="namespace-picker">
        <input
          className="filter-search-input"
          aria-label="Find a namespace"
          placeholder="Find a namespace…"
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          spellCheck={false}
          autoComplete="off"
        />
        <div className="namespace-picker-list" role="radiogroup" aria-label="Namespaces">
          {namespaces.map((n) => (
            <label key={n.name} className="filter-row">
              <input
                type="radio"
                name="fork-namespace"
                checked={picked === n.name}
                onChange={() => onPick(n.name)}
              />
              <span
                className="filter-value"
                title={n.displayName ? `Namespace ${n.name}` : undefined}
              >
                {n.displayName ?? n.name}
              </span>
              <span className="filter-count" title="Runs this month">
                {formatCount(n.usage.runsStarted)}
              </span>
            </label>
          ))}
          {found.status === "ready" && namespaces.length === 0 && (
            <p className="muted namespace-picker-empty">
              No namespace starts with “{typed.trim()}”.
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

function ForkForm({
  workflow,
  label,
  onDone,
}: {
  workflow: Workflow;
  label: string;
  onDone: (fork: Workflow) => void;
}) {
  const [namespace, setNamespace] = useState<string | null>(null);
  const fork = useForkWorkflow(workflow.id);
  return (
    <DialogForm
      title={`Fork ${label}`}
      meta={
        <span className="num">v{workflow.versions[0]?.version ?? workflow.version.version}</span>
      }
      description={
        <p>
          The fork keeps its name, so that namespace's runs that name it get the fork from then on.
          It starts with a copy of its suite, without the tests recorded from other namespaces'
          runs, and can pull its new versions.
        </p>
      }
      submit="Fork"
      pendingLabel="Forking…"
      pending={fork.pending}
      disabled={namespace === null}
      error={fork.error}
      onSubmit={() => {
        if (namespace !== null) void fork.run(namespace, onDone);
      }}
    >
      <NamespacePicker picked={namespace} onPick={setNamespace} />
    </DialogForm>
  );
}

function PullForm({
  workflow,
  sourceLabel,
  sourceVersion,
  onDone,
}: {
  workflow: Workflow;
  sourceLabel: string;
  sourceVersion: number;
  onDone: () => void;
}) {
  const pull = usePullWorkflow(workflow.id);
  const failed =
    pull.error instanceof ApiError && pull.error.slug === "tests-failed"
      ? (pull.error.problem.tests ?? []).filter((t) => !t.passed)
      : [];
  return (
    <DialogForm
      title={`Pull ${sourceLabel} v${sourceVersion}?`}
      description={
        <p>
          {sourceLabel} v{sourceVersion} becomes this fork's next version once its own suite passes
          against it: its code, interface, action set and secrets. If a test fails, nothing changes.
        </p>
      }
      submit="Pull"
      pendingLabel="Pulling…"
      pending={pull.pending}
      error={pull.error}
      errorTitle={failed.length > 0 ? "Its suite did not pass" : undefined}
      onSubmit={() => void pull.run(undefined, onDone)}
    >
      {failed.length > 0 && (
        <ul className="pull-failures" aria-label="Tests that failed">
          {failed.map((t) => (
            <li key={t.name}>
              <span className="mono">{t.name}</span>
              {t.failures[0] && <span className="dim"> · {t.failures[0]}</span>}
            </li>
          ))}
        </ul>
      )}
    </DialogForm>
  );
}

type Open = "edit" | "promote" | "fork" | "pull" | null;

/**
 * The header's controls of a workflow: Edit always; Promote for a namespace's workflow that is not
 * a fork; Fork for an organization's; Pull for a fork whose source has a newer version.
 */
export function WorkflowControls({
  workflow,
  label,
  sourceLabel,
}: {
  workflow: Workflow;
  /** How it shows: `triage`. */
  label: string;
  /** How a fork's source shows. */
  sourceLabel: string | null;
}) {
  const access = useAccess();
  const navigate = useNavigate();
  const [open, setOpen] = useState<Open>(null);
  const source = workflow.source ?? null;
  // A fork's source, at its latest version, to know whether there is something to pull.
  const sourceRead = useWorkflow(source && access.can("workflows:write") ? source.workflow : null);
  const sourceLatest = sourceRead.data?.versions[0]?.version ?? null;
  if (!access.can("workflows:write")) return null;
  const close = (o: boolean) => {
    if (!o) setOpen(null);
  };
  const canPull = source !== null && sourceLatest !== null && sourceLatest > source.version;
  return (
    <>
      <button type="button" className="btn" onClick={() => setOpen("edit")}>
        <PenLine size={14} aria-hidden="true" /> Edit
      </button>
      {workflow.namespace && !source && (
        <button
          type="button"
          className="btn"
          title="Move it to the organization"
          onClick={() => setOpen("promote")}
        >
          <ArrowUpToLine size={14} aria-hidden="true" /> Promote
        </button>
      )}
      {!workflow.namespace && (
        <button
          type="button"
          className="btn"
          title="Fork it into a namespace, to change it for that end user"
          onClick={() => setOpen("fork")}
        >
          <GitFork size={14} aria-hidden="true" /> Fork
        </button>
      )}
      {canPull && (
        <button
          type="button"
          className="btn"
          title={`Take ${sourceLabel ?? "its source"} v${sourceLatest} as its next version`}
          onClick={() => setOpen("pull")}
        >
          <GitPullRequestArrow size={14} aria-hidden="true" /> Pull v{sourceLatest}
        </button>
      )}
      <FormDialog open={open === "edit"} onOpenChange={close}>
        <EditForm workflow={workflow} onDone={() => setOpen(null)} />
      </FormDialog>
      <FormDialog open={open === "promote"} onOpenChange={close}>
        <PromoteForm workflow={workflow} label={label} onDone={() => setOpen(null)} />
      </FormDialog>
      <FormDialog open={open === "fork"} onOpenChange={close}>
        <ForkForm
          workflow={workflow}
          label={label}
          onDone={(fork) => {
            setOpen(null);
            void navigate(workflowPath(fork.id));
          }}
        />
      </FormDialog>
      {canPull && sourceLatest !== null && (
        <FormDialog open={open === "pull"} onOpenChange={close}>
          <PullForm
            workflow={workflow}
            sourceLabel={sourceLabel ?? "Its source"}
            sourceVersion={sourceLatest}
            onDone={() => setOpen(null)}
          />
        </FormDialog>
      )}
    </>
  );
}
