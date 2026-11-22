// Scripts: every distinct script executed (content-addressed, so deduplicated), with its runs by
// outcome, the tools it called and the labels it ran with. Searched with the query bar of the
// runs, which reads labels and one tool here, what `GET /v1/scripts` filters by, and one
// namespace, sent as `Rewind-Namespace`: the scripts of one end user, counted over their runs.
import { useMemo } from "react";
import { Link, useNavigate } from "react-router";
import { Ago, Digest, Empty, LabelList, Loading, ProblemView } from "../components/bits";
import { NamespaceName } from "../components/NamespaceName";
import { PageHeader } from "../components/PageHeader";
import { QueryBar } from "../components/QueryBar";
import { StatusGlyph } from "../components/Status";
import { legacyScriptsQuery, RANGES, SCRIPTS_LANGUAGE, scriptsSearch } from "../model/filters";
import { formatCount } from "../model/format";
import { fieldTerm, joinQuery, labelTerm, queryLink, splitTerms, withTerm } from "../model/query";
import { RUN_STATUSES, type RunCounts, type ScriptSummary } from "../model/types";
import { useRunStats } from "../state/aggregates";
import { type ScriptList, useScriptList } from "../state/scripts";
import { useListQuery, useRunQueryValues } from "./search";
import { TableToolbar } from "./TableTop";

const LEGACY_NAMES = ["label", "tool"] as const;

export function totalRuns(counts: RunCounts): number {
  return RUN_STATUSES.reduce((sum, s) => sum + counts[s], 0);
}

/** Runs by status: a bar to compare at a glance, counts with glyphs to read. */
export function Outcomes({
  counts,
  digest,
  scope = "",
}: {
  counts: RunCounts;
  digest?: string;
  /** A term the links to the runs add: the namespace the counts are of. */
  scope?: string;
}) {
  const total = totalRuns(counts);
  const present = RUN_STATUSES.filter((s) => counts[s] > 0);
  const title = present.map((s) => `${counts[s]} ${s}`).join(", ");
  return (
    <span className="outcomes" title={title}>
      <span className="outcome-bar" aria-hidden="true">
        {present.map((s) => (
          <span key={s} className={`seg-${s}`} style={{ flexGrow: counts[s] }} />
        ))}
        {total === 0 && <span className="seg-pending" style={{ flexGrow: 1 }} />}
      </span>
      <span className="outcome-counts">
        {present.map((s) =>
          digest ? (
            <Link
              key={s}
              to={queryLink("/observability", joinQuery(`status:${s} script:${digest}`, scope))}
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

function topLabels(labels: Record<string, number> | undefined, n: number): string[] {
  return Object.entries(labels ?? {})
    .sort(([a, x], [b, y]) => y - x || a.localeCompare(b))
    .slice(0, n)
    .map(([pair]) => pair);
}

function ScriptRow({
  script,
  scope,
  onPickLabel,
  onPickTool,
}: {
  script: ScriptSummary;
  scope: string;
  onPickLabel: (p: string) => void;
  onPickTool: (t: string) => void;
}) {
  const navigate = useNavigate();
  const shown = script.tools.slice(0, 3);
  return (
    <tr
      className="link"
      onClick={(e) => {
        if ((e.target as HTMLElement).closest("a, button")) return;
        navigate(`/scripts/${script.digest}`);
      }}
    >
      <td className="script-cell">
        <Link to={`/scripts/${script.digest}`} className="headline mono" title={script.headline}>
          {script.headline || <span className="muted">(empty)</span>}
        </Link>
        <Digest digest={script.digest} />
      </td>
      <td className="right">
        <Link
          to={queryLink("/observability", joinQuery(`script:${script.digest}`, scope))}
          className="num"
          title="Its runs"
        >
          {formatCount(totalRuns(script.runs))}
        </Link>
      </td>
      <td>
        <Outcomes counts={script.runs} digest={script.digest} scope={scope} />
      </td>
      <td className="wrap">
        <span className="labels">
          {shown.map((tool) => (
            <button
              key={tool}
              type="button"
              className="label"
              title={`Only scripts that called ${tool}`}
              onClick={() => onPickTool(tool)}
            >
              {tool}
            </button>
          ))}
          {script.tools.length > shown.length && (
            <span className="muted" title={script.tools.slice(3).join(", ")}>
              +{script.tools.length - shown.length}
            </span>
          )}
          {script.tools.length === 0 && <span className="muted">–</span>}
        </span>
      </td>
      <td className="wrap">
        <LabelList
          labels={topLabels(script.labels, 3)}
          counts={script.labels ?? {}}
          onPick={onPickLabel}
        />
      </td>
      <td className="dim">
        <Ago ms={Date.parse(script.lastRunAt)} />
        <span className="cell-sub">
          first <Ago ms={Date.parse(script.firstRunAt)} />
        </span>
      </td>
    </tr>
  );
}

/**
 * The table of scripts, shared by Scripts and a namespace's page: each script's runs by outcome,
 * its tools, its labels, when it last ran. `scope` is the term its links to the runs add.
 */
export function ScriptTable({
  scripts,
  scope,
  onPickLabel,
  onPickTool,
  empty = "No script matches this search.",
}: {
  scripts: ScriptList;
  scope: string;
  onPickLabel: (pair: string) => void;
  onPickTool: (tool: string) => void;
  empty?: string;
}) {
  if (scripts.status === "pending") return <Loading>Loading scripts…</Loading>;
  if (scripts.status === "error")
    return <ProblemView error={scripts.error} title="The scripts could not be read" />;
  const list = scripts.rows;
  return (
    <>
      <TableToolbar
        pending={scripts.pending}
        onShow={scripts.showPending}
        setAtTop={scripts.setAtTop}
      >
        {list.length > 0 && (
          <span className="num muted">
            {formatCount(list.length)} script{list.length === 1 ? "" : "s"}
            {scripts.hasMore ? " shown" : ""}
          </span>
        )}
      </TableToolbar>
      <div
        className={`table-wrap${scripts.stale ? " is-stale" : ""}`}
        aria-busy={scripts.stale || undefined}
      >
        <table className="grid">
          <thead>
            <tr>
              <th>Script</th>
              <th className="right">Runs</th>
              <th>Outcomes</th>
              <th>Tools</th>
              <th>Labels</th>
              <th>Last run</th>
            </tr>
          </thead>
          <tbody>
            {list.map((s) => (
              <ScriptRow
                key={s.digest}
                script={s}
                scope={scope}
                onPickLabel={onPickLabel}
                onPickTool={onPickTool}
              />
            ))}
          </tbody>
        </table>
        {list.length === 0 && <Empty>{empty}</Empty>}
        {scripts.hasMore && (
          <div className="list-foot">
            <span className="num">{formatCount(list.length)} scripts shown</span>
            <button
              type="button"
              className="btn small"
              disabled={scripts.loadingMore}
              onClick={scripts.loadMore}
            >
              {scripts.loadingMore ? "Loading…" : "Load more"}
            </button>
          </div>
        )}
      </div>
    </>
  );
}

/**
 * What the bar suggests: the namespaces, labels and tools of the last 30 days' runs, and the
 * namespaces whose name starts with what is typed.
 */
function useScriptFacets() {
  const facets = useRunStats("scripts-facets", () => ({
    since: new Date(Date.now() - RANGES["30d"]).toISOString(),
    top: 20,
  }));
  const lookup = useRunQueryValues();
  const data = facets.data?.facets;
  return useMemo(
    () => ({
      // Tools are offered by name: the list takes one tool, never a pattern.
      facets: data ? { ...data, tools: [] } : undefined,
      values: {
        ...lookup.values,
        tool: (data?.tools ?? []).map((t) => ({ value: t.value, count: t.runs })),
      },
      onTyping: lookup.onTyping,
    }),
    [data, lookup.values, lookup.onTyping],
  );
}

export function ScriptsView() {
  const { q, setQ } = useListQuery(legacyScriptsQuery, LEGACY_NAMES);
  const search = scriptsSearch(q);
  const scripts = useScriptList({
    query: search.query,
    namespace: search.namespace,
    enabled: search.fault === null,
  });
  const suggestions = useScriptFacets();
  const addTerm = (term: string) => setQ(withTerm(q, term));
  const scope = search.namespace ? fieldTerm("namespace", search.namespace) : "";

  return (
    <div className="page">
      <PageHeader title="Scripts" />
      <p className="page-intro">
        {search.namespace ? (
          <>
            The sources the runs of <NamespaceName name={search.namespace} /> executed, the most
            recently run first; their runs are that end user's.
          </>
        ) : (
          "Every distinct source executed, the most recently run first."
        )}
      </p>
      <QueryBar
        value={q}
        onSubmit={setQ}
        language={SCRIPTS_LANGUAGE}
        facets={suggestions.facets}
        values={suggestions.values}
        onTyping={suggestions.onTyping}
        fault={search.fault}
        placeholder="namespace:alice @agent:triage tool:github.close_issue"
      />
      {search.fault ? (
        <Empty>Fix the query to see the scripts: the term at fault is underlined above.</Empty>
      ) : (
        <ScriptTable
          scripts={scripts}
          scope={scope}
          onPickLabel={(pair) => {
            const at = pair.indexOf(":");
            addTerm(labelTerm(pair.slice(0, at), pair.slice(at + 1)));
          }}
          onPickTool={(tool) =>
            setQ(
              joinQuery(
                ...splitTerms(q)
                  .filter((t) => !(t.kind === "field" && t.field === "tool"))
                  .map((t) => t.raw),
                fieldTerm("tool", tool),
              ),
            )
          }
        />
      )}
    </div>
  );
}
