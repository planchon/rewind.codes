// One script: its title (the one a person gave it, else its comment's), its source, the suites of
// the workflow versions it is the script of, and its runs in Observability's table forced to the
// script. Whoever holds `scripts:write` gives it a title and a description, or removes them.
import { PenLine } from "lucide-react";
import { useState } from "react";
import { Link, useParams } from "react-router";
import { CopyButton, Loading, ProblemView } from "../components/bits";
import { CodeView } from "../components/CodeView";
import { DialogForm, FormDialog } from "../components/FormDialog";
import { PageHeader } from "../components/PageHeader";
import { shortDigest } from "../model/format";
import { fieldTerm } from "../model/query";
import type { Script } from "../model/types";
import { useAccess } from "../state/access";
import { useScript, useSetScriptTitle } from "../state/scripts";
import { Field } from "./org/bits";
import { ScopedRuns } from "./ScopedRuns";
import { ScriptSuites } from "./testing/ScriptSuites";

const MAX_TITLE = 120;
const MAX_DESCRIPTION = 2_000;

function TitleEditor({ script, onDone }: { script: Script; onDone: () => void }) {
  const set = useSetScriptTitle(script.digest);
  const byPerson = script.titleSource === "person";
  const [title, setTitle] = useState(script.title ?? "");
  const [description, setDescription] = useState(script.description ?? "");
  const trimmed = title.trim();
  const tooLong = trimmed.length > MAX_TITLE || description.trim().length > MAX_DESCRIPTION;
  return (
    <DialogForm
      title="Title of the script"
      meta={<span className="mono">{shortDigest(script.digest)}</span>}
      description={
        <>
          For your organization, over the comment at the top of the script. It names the script
          wherever it shows, and the runs started after it, as the middle of their handle.
        </>
      }
      submit="Save the title"
      pending={set.pending}
      disabled={trimmed === "" || tooLong}
      error={set.error}
      onSubmit={() => {
        void set.run({ title: trimmed, description: description.trim() || null }, onDone);
      }}
    >
      <Field label="Title" hint={`At most ${MAX_TITLE} characters.`}>
        {(props) => (
          <input
            {...props}
            className="input"
            value={title}
            maxLength={MAX_TITLE}
            onChange={(e) => setTitle(e.target.value)}
            placeholder="Triage new GitHub issues"
            autoComplete="off"
          />
        )}
      </Field>
      <Field label="Description" hint="What it does, for whoever reads its runs. Optional.">
        {(props) => (
          <textarea
            {...props}
            className="input"
            rows={4}
            value={description}
            maxLength={MAX_DESCRIPTION}
            onChange={(e) => setDescription(e.target.value)}
          />
        )}
      </Field>
      {byPerson && (
        <button
          type="button"
          className="link-btn"
          disabled={set.pending}
          onClick={() => void set.run({ title: null }, onDone)}
        >
          Remove this title: the comment's shows again
        </button>
      )}
    </DialogForm>
  );
}

export function ScriptView() {
  const { digest = "" } = useParams();
  const script = useScript(digest);
  const access = useAccess();
  const [editing, setEditing] = useState(false);
  const data = script.data;
  const headline = data?.source.split("\n").find((l) => l.trim() !== "");
  const title = data?.title ?? null;

  return (
    <div className="page">
      <PageHeader
        crumbs={<Link to="/scripts">Scripts</Link>}
        title={title ?? <span className="muted">Untitled script</span>}
        info={
          data && (
            <>
              {data.description && <p className="info-card-head">{data.description}</p>}
              <p>
                {data.titleSource === "person"
                  ? "Its title was given for your organization."
                  : data.titleSource === "comment"
                    ? "Its title is the first line of the comment at its top."
                    : "It has no title: give it one, or start its source with a comment."}
              </p>
              <p className="copyable">
                <span className="mono">{digest}</span>
                <CopyButton text={digest} label="Copy the script's digest" />
              </p>
              {headline && !title && <p className="mono">{headline}</p>}
            </>
          )
        }
        actions={
          access.can("scripts:write") &&
          data && (
            <button type="button" className="btn" onClick={() => setEditing(true)}>
              <PenLine size={14} aria-hidden="true" /> {title ? "Edit the title" : "Give a title"}
            </button>
          )
        }
      />
      <FormDialog open={editing && data !== undefined} onOpenChange={setEditing}>
        {data && <TitleEditor script={data} onDone={() => setEditing(false)} />}
      </FormDialog>
      <section className="panel">
        <header className="panel-head">
          <h2>Source</h2>
          <span className="muted">
            script.ts{data ? ` · ${data.source.split("\n").length} lines` : ""}
          </span>
        </header>
        {script.isPending ? (
          <Loading>Loading the source…</Loading>
        ) : script.isError ? (
          <div className="panel-body">
            <ProblemView error={script.error} title="The source could not be read" />
          </div>
        ) : (
          <CodeView source={script.data.source} />
        )}
      </section>
      <ScriptSuites digest={digest} />
      <ScopedRuns
        name={`script|${digest}`}
        label="Runs of this script"
        title="Runs of this script"
        scope={fieldTerm("script", digest)}
        locked={[
          {
            label: <>Script {title ?? shortDigest(digest)}</>,
            title: `Only the runs of ${digest}`,
          },
        ]}
        limit={50}
        hideScript
        empty="No run of this script yet."
      />
    </div>
  );
}
