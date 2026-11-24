// Scripts: every distinct script executed (content-addressed, so deduplicated), with its runs by
// outcome and the tools it called since its first run, from the hourly counters (archi/05,
// "Statistics"), which know no labels; no window of time, a script being what it is whenever it
// ran. Searched with the query bar of the runs, which reads one
// tool here, what `GET /v1/scripts` filters by, and one namespace, sent as `Rewind-Namespace`: the
// scripts of one end user, counted over their runs. In a namespace's scope, that namespace's.
import { useMemo } from "react";
import { Link, useNavigate } from "react-router";
import { useAt } from "../app/scope";
import { Ago, Empty, Loading, ProblemView } from "../components/bits";
import { NamespaceName } from "../components/NamespaceName";
import { PageHeader } from "../components/PageHeader";
import { QueryBar } from "../components/QueryBar";
import { StatusGlyph } from "../components/Status";
import { TableScroll } from "../components/TableScroll";
import { legacyScriptsQuery, RANGES, SCRIPTS_LANGUAGE, scriptsSearch } from "../model/filters";
import { formatCount } from "../model/format";
import { fieldTerm, joinQuery, queryLink, splitTerms } from "../model/query";
import { RUN_STATUSES, type RunCounts, type ScriptSummary } from "../model/types";
import { useRunStats } from "../state/aggregates";
import { useScopeNamespace } from "../state/scope";
import { type ScriptList, useScriptList } from "../state/scripts";
import { useListQuery, useRunQueryValues } from "./search";
import { TableTop } from "./TableTop";

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
  const at = useAt();
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
              to={queryLink(at("/observability"), joinQuery(`status:${s} script:${digest}`, scope))}
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

function ScriptRow({
  script,
  scope,
  onPickTool,
}: {
  script: ScriptSummary;
  scope: string;
  onPickTool: (t: string) => void;
}) {
  const navigate = useNavigate();
  const at = useAt();
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
        {script.title ? (
          <Link
            to={`/scripts/${script.digest}`}
            className="headline script-title"
            title={script.description ?? script.title}
          >
            {script.title}
          </Link>
        ) : (
          <Link to={`/scripts/${script.digest}`} className="headline" title={script.headline}>
            <span className="muted">Untitled script</span>
          </Link>
        )}
        <span className="cell-sub mono" title={script.digest}>
          {script.headline || "(empty)"}
        </span>
      </td>
      <td className="right">
        <Link
          to={queryLink(at("/observability"), joinQuery(`script:${script.digest}`, scope))}
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
 * The table of scripts: each script's runs by outcome, its tools, when it last ran. `scope` is the
 * term its links to the runs add; in a namespace's scope, they open that scope's runs.
 */
export function ScriptTable({
  scripts,
  scope,
  onPickTool,
  empty = "No script matches this search.",
}: {
  scripts: ScriptList;
  scope: string;
  onPickTool: (tool: string) => void;
  empty?: string;
}) {
  if (scripts.status === "pending") return <Loading>Loading scripts…</Loading>;
  if (scripts.status === "error")
    return <ProblemView error={scripts.error} title="The scripts could not be read" />;
  const list = scripts.rows;
  return (
    <div className="live-table">
      <TableTop
        pending={scripts.pending}
        onShow={scripts.showPending}
        setAtTop={scripts.setAtTop}
      />
      <TableScroll stale={scripts.stale}>
        <table className="grid">
          <thead>
            <tr>
              <th>Script</th>
              <th className="right">Runs</th>
              <th>Outcomes</th>
              <th>Tools</th>
              <th>Last run</th>
            </tr>
          </thead>
          <tbody>
            {list.map((s) => (
              <ScriptRow key={s.digest} script={s} scope={scope} onPickTool={onPickTool} />
            ))}
          </tbody>
        </table>
        {list.length === 0 && <Empty>{empty}</Empty>}
        {scripts.hasMore && (
          <div className="list-foot">
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
      </TableScroll>
    </div>
  );
}

/**
 * What the bar suggests: the namespaces and tools of the last 30 days' runs, and the namespaces
 * whose name starts with what is typed.
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
  const scoped = useScopeNamespace();
  const search = scriptsSearch(q);
  // The scope's namespace, else the one the query names.
  const namespace = scoped ?? search.namespace;
  const scripts = useScriptList({
    query: search.query,
    namespace,
    enabled: search.fault === null,
  });
  const suggestions = useScriptFacets();
  const scope = scoped === null && namespace ? fieldTerm("namespace", namespace) : "";

  return (
    <div className="page">
      <PageHeader
        title="Scripts"
        info={
          <p>
            Every distinct source the runs
            {namespace ? (
              <>
                {" "}
                of <NamespaceName name={namespace} />
              </>
            ) : null}{" "}
            executed, the most recently run first, with their runs by outcome and the tools they
            called.
          </p>
        }
      />
      <QueryBar
        value={q}
        onSubmit={setQ}
        language={SCRIPTS_LANGUAGE}
        facets={suggestions.facets}
        values={suggestions.values}
        onTyping={suggestions.onTyping}
        fault={search.fault}
        placeholder={
          scoped === null ? "namespace:alice tool:github.close_issue" : "tool:github.close_issue"
        }
      />
      {search.fault ? (
        <Empty>Fix the query to see the scripts: the term at fault is underlined above.</Empty>
      ) : (
        <ScriptTable
          scripts={scripts}
          scope={scope}
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
