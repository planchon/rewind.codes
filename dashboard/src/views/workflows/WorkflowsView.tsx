// Workflows (archi/05, "Workflows"): every workflow of the organization and of its namespaces, the
// most recently run first, each as it shows (its name, its comment's first line, or its short id)
// with its scope, its latest version, and its runs by outcome and the actions they called over a
// window, from the hourly counters (archi/05, "Statistics"), which know no labels. Searched with
// the query bar of the runs, which reads one namespace here, sent as `Rewind-Namespace` (that end
// user's workflows and the organization's, counted over its runs), and actions, kept in the browser.
import { GitFork } from "lucide-react";
import { useMemo } from "react";
import { Link, useNavigate } from "react-router";
import { Ago, Empty, Loading, ProblemView } from "../../components/bits";
import { HoverCard } from "../../components/HoverCard";
import { PageHeader } from "../../components/PageHeader";
import { QueryBar } from "../../components/QueryBar";
import { StatusGlyph } from "../../components/Status";
import { TableScroll } from "../../components/TableScroll";
import {
  legacyWorkflowsQuery,
  RANGES,
  type RangeKey,
  WORKFLOWS_LANGUAGE,
  workflowsSearch,
} from "../../model/filters";
import { formatCount } from "../../model/format";
import { fieldTerm, joinQuery, queryLink, splitTerms } from "../../model/query";
import { RUN_COUNT_KEYS, type RunCounts, type WorkflowSummary } from "../../model/types";
import { isNamed, shortIdOf, workflowLabel, workflowPath } from "../../model/workflows";
import { useAccess } from "../../state/access";
import { useRunStats } from "../../state/aggregates";
import { useWorkflowLabels, useWorkflowList, type WorkflowList } from "../../state/workflows";
import { Seg } from "../FilterBits";
import { useListQuery, useRunQueryValues, useWindow, WINDOW_WORDS, WINDOWS } from "../search";
import { TableTop } from "../TableTop";
import { firstLine, Scope } from "./bits";

const LEGACY_NAMES = ["label", "tool"] as const;
/** The actions a row shows whole before "+N". */
const ACTIONS_SHOWN = 3;

export function totalRuns(counts: RunCounts): number {
  return RUN_COUNT_KEYS.reduce((sum, s) => sum + (counts[s] ?? 0), 0);
}

/** Runs by status: a bar to compare at a glance, counts with glyphs to read. */
export function Outcomes({
  counts,
  workflow,
  scope = "",
}: {
  counts: RunCounts;
  /** The workflow whose runs the counts link to: its id. */
  workflow?: string;
  /** A term the links to the runs add: the namespace the counts are of. */
  scope?: string;
}) {
  const total = totalRuns(counts);
  const present = RUN_COUNT_KEYS.filter((s) => (counts[s] ?? 0) > 0);
  const title = present.map((s) => `${counts[s]} ${s}`).join(", ");
  return (
    <span className="outcomes" title={title}>
      <span className="outcome-bar" aria-hidden="true">
        {present.map((s) => (
          <span key={s} className={`seg-${s}`} style={{ flexGrow: counts[s] }} />
        ))}
        {total === 0 && <span className="seg-queued" style={{ flexGrow: 1 }} />}
      </span>
      <span className="outcome-counts">
        {present.map((s) =>
          workflow ? (
            <Link
              key={s}
              to={queryLink(
                "/runs",
                joinQuery(`status:${s}`, fieldTerm("workflow", workflow), scope),
              )}
              className={`status ${s}`}
              title={`${counts[s]} ${s}`}
            >
              <StatusGlyph status={s} size={11} />
              <span className="num">{formatCount(counts[s])}</span>
            </Link>
          ) : (
            <span key={s} className={`status ${s}`} title={`${counts[s]} ${s}`}>
              <StatusGlyph status={s} size={11} />
              <span className="num">{formatCount(counts[s])}</span>
            </span>
          ),
        )}
      </span>
    </span>
  );
}

/** What shows under a workflow's label: the first line of its description, or of its comment's. */
function subline(w: WorkflowSummary): string | null {
  const comment = w.latest.comment ?? null;
  return (
    firstLine(w.description) ?? (w.name ? comment?.title : firstLine(comment?.description)) ?? null
  );
}

function Actions({
  actions,
  onPick,
}: {
  actions: readonly string[];
  onPick: (action: string) => void;
}) {
  if (actions.length === 0) return <span className="muted">–</span>;
  const shown = actions.slice(0, ACTIONS_SHOWN);
  const hidden = actions.length - shown.length;
  const button = (action: string) => (
    <button
      key={action}
      type="button"
      className="label"
      title={`Only the workflows that called ${action}`}
      onClick={() => onPick(action)}
    >
      {action}
    </button>
  );
  return (
    <span className="labels">
      {shown.map(button)}
      {hidden > 0 && (
        <HoverCard
          label={`The ${actions.length} actions its runs called`}
          className="labels-card"
          trigger={
            <button type="button" className="label more" aria-label={`${hidden} more actions`}>
              +{hidden}
            </button>
          }
        >
          <p className="card-title">{actions.length} actions</p>
          <span className="labels">{actions.map(button)}</span>
        </HoverCard>
      )}
    </span>
  );
}

function WorkflowRow({
  workflow,
  scope,
  counted,
  sourceLabel,
  onPickAction,
}: {
  workflow: WorkflowSummary;
  scope: string;
  /** The caller reads the counters: its runs, outcomes and actions show. */
  counted: boolean;
  sourceLabel: string | null;
  onPickAction: (action: string) => void;
}) {
  const navigate = useNavigate();
  const to = workflowPath(workflow.id);
  const label = workflowLabel({ ...workflow, comment: workflow.latest.comment });
  const named = isNamed(workflow) || Boolean(workflow.latest.comment);
  const sub = subline(workflow);
  const source = workflow.source ?? null;
  return (
    <tr
      className="link"
      onClick={(e) => {
        if ((e.target as HTMLElement).closest("a, button")) return;
        void navigate(to);
      }}
    >
      <td className="wf-cell">
        <span className="wf-cell-head">
          <Link
            to={to}
            className={named ? "headline wf-name" : "headline wf-name mono"}
            title={`${workflow.id}\ncode ${workflow.latest.codeDigest}`}
          >
            {label}
          </Link>
          <span className="muted num" title="Its latest version">
            v{workflow.latest.version}
          </span>
          {source && (
            <span
              className="tag"
              title={`Fork of ${sourceLabel ?? shortIdOf(source.workflow)} v${source.version}`}
            >
              <span className="glyph" aria-hidden="true">
                <GitFork size={11} strokeWidth={2.5} />
              </span>
              fork
            </span>
          )}
        </span>
        {sub && <span className="cell-sub">{sub}</span>}
      </td>
      <td>
        <Scope namespace={workflow.namespace} />
      </td>
      {counted && workflow.runs ? (
        <>
          <td className="right">
            <Link
              to={queryLink("/runs", joinQuery(fieldTerm("workflow", workflow.id), scope))}
              className="num"
              title="Its runs"
            >
              {formatCount(totalRuns(workflow.runs))}
            </Link>
          </td>
          <td>
            <Outcomes counts={workflow.runs} workflow={workflow.id} scope={scope} />
          </td>
          <td className="wrap">
            <Actions actions={workflow.actions} onPick={onPickAction} />
          </td>
        </>
      ) : null}
      <td className="dim">
        {workflow.lastRunAt ? (
          <>
            <Ago ms={Date.parse(workflow.lastRunAt)} />
            {workflow.firstRunAt && (
              <span className="cell-sub">
                first <Ago ms={Date.parse(workflow.firstRunAt)} />
              </span>
            )}
          </>
        ) : (
          <span className="muted">–</span>
        )}
      </td>
    </tr>
  );
}

/**
 * The table of workflows, shared by Workflows and a namespace's page: each workflow's scope and
 * latest version, its runs by outcome, the actions they called, when it last ran. `scope` is the
 * term its links to the runs add; `keeps` keeps the rows of the actions the query names.
 */
export function WorkflowTable({
  workflows,
  scope,
  onPickAction,
  keeps,
  empty = "No workflow matches this search.",
}: {
  workflows: WorkflowList;
  scope: string;
  onPickAction: (action: string) => void;
  keeps?: (actions: readonly string[]) => boolean;
  empty?: string;
}) {
  const access = useAccess();
  const labelOf = useWorkflowLabels();
  if (workflows.status === "pending") return <Loading>Loading workflows…</Loading>;
  if (workflows.status === "error")
    return <ProblemView error={workflows.error} title="The workflows could not be read" />;
  const counted = access.statistics;
  const list = keeps ? workflows.rows.filter((w) => keeps(w.actions)) : workflows.rows;
  return (
    <div className="live-table">
      <TableTop
        pending={workflows.pending}
        onShow={workflows.showPending}
        setAtTop={workflows.setAtTop}
      />
      <TableScroll stale={workflows.stale}>
        <table className="grid">
          <thead>
            <tr>
              <th>Workflow</th>
              <th>Scope</th>
              {counted && (
                <>
                  <th className="right">Runs</th>
                  <th>Outcomes</th>
                  <th>Actions</th>
                </>
              )}
              <th>Last run</th>
            </tr>
          </thead>
          <tbody>
            {list.map((w) => (
              <WorkflowRow
                key={w.id}
                workflow={w}
                scope={scope}
                counted={counted}
                sourceLabel={w.source ? (labelOf(w.source.workflow) ?? null) : null}
                onPickAction={onPickAction}
              />
            ))}
          </tbody>
        </table>
        {list.length === 0 && <Empty>{empty}</Empty>}
        {workflows.hasMore && (
          <div className="list-foot">
            <button
              type="button"
              className="btn small"
              disabled={workflows.loadingMore}
              onClick={workflows.loadMore}
            >
              {workflows.loadingMore ? "Loading…" : "Load more"}
            </button>
          </div>
        )}
      </TableScroll>
    </div>
  );
}

/**
 * What the bar suggests: the namespaces and actions of the last 30 days' runs, and the namespaces
 * whose name starts with what is typed.
 */
function useWorkflowFacets() {
  const access = useAccess();
  const facets = useRunStats("workflows-facets", () => ({
    since: new Date(Date.now() - RANGES["30d"]).toISOString(),
    top: 20,
  }));
  const lookup = useRunQueryValues();
  const data = access.statistics ? facets.data?.facets : undefined;
  return useMemo(
    () => ({ facets: data, values: lookup.values, onTyping: lookup.onTyping }),
    [data, lookup.values, lookup.onTyping],
  );
}

export function WorkflowsView() {
  const { q, setQ } = useListQuery(legacyWorkflowsQuery, LEGACY_NAMES);
  const [win, setWin] = useWindow();
  const search = workflowsSearch(q);
  const workflows = useWorkflowList({
    query: search.query,
    range: win,
    namespace: search.namespace,
    enabled: search.fault === null,
  });
  const suggestions = useWorkflowFacets();
  const scope = search.namespace ? fieldTerm("namespace", search.namespace) : "";

  return (
    <div className="page">
      <PageHeader
        title="Workflows"
        info={
          <p>
            Every workflow of the organization and of its namespaces, the most recently run first,
            with its runs in {WINDOW_WORDS[win]}. A workflow shows as its name, else the first line
            of the comment at the top of its code, else its short id. With namespace:, one end
            user's workflows and the organization's, counted over that end user's runs.
          </p>
        }
        actions={
          <Seg<RangeKey>
            label="Window"
            value={win}
            onChange={setWin}
            options={WINDOWS.map((w) => ({ value: w, label: w, title: WINDOW_WORDS[w] }))}
          />
        }
      />
      <QueryBar
        value={q}
        onSubmit={setQ}
        language={WORKFLOWS_LANGUAGE}
        facets={suggestions.facets}
        values={suggestions.values}
        onTyping={suggestions.onTyping}
        fault={search.fault}
        placeholder="namespace:alice action:github.close_issue"
      />
      {search.fault ? (
        <Empty>Fix the query to see the workflows: the term at fault is underlined above.</Empty>
      ) : (
        <WorkflowTable
          workflows={workflows}
          scope={scope}
          keeps={search.filtered ? search.keeps : undefined}
          empty={`No workflow matches this search in ${WINDOW_WORDS[win]}.`}
          onPickAction={(action) =>
            setQ(
              joinQuery(
                ...splitTerms(q)
                  .filter((t) => !(t.kind === "field" && t.field === "action" && !t.negated))
                  .map((t) => t.raw),
                fieldTerm("action", action),
              ),
            )
          }
        />
      )}
    </div>
  );
}
