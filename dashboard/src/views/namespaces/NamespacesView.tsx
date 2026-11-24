// Namespaces: the end users of the organization and what they use of their limits this month.
// First the alerts of the month, then the organization's ceiling (the operator's) with its
// totals, then, folding under it, the
// default limits and the namespaces, sorted by CPU, runs or scripts of the month, so that an end
// user who abuses the product comes first; the query bar of the runs, which reads the start of a
// name here, and a filter of those at 80 % or more of a limit. Live: their use moves with their
// runs (`state/namespaces.ts`).
import { ChevronDown, ChevronRight, Pencil } from "lucide-react";
import { type ReactNode, useMemo, useState } from "react";
import { Link } from "react-router";
import { Ago, Empty, Loading, ProblemView } from "../../components/bits";
import { FormDialog } from "../../components/FormDialog";
import { NamespaceName } from "../../components/NamespaceName";
import { PageHeader } from "../../components/PageHeader";
import { QueryBar } from "../../components/QueryBar";
import { TableScroll } from "../../components/TableScroll";
import { legacyNamespacesQuery, NAMESPACES_LANGUAGE, namespacesSearch } from "../../model/filters";
import { formatCount } from "../../model/format";
import {
  formatQuota,
  limitOf,
  METERED,
  monthLabel,
  monthOf,
  QUOTA_FIELDS,
  QUOTA_KEYS,
  QUOTA_NAMES,
  shortDate,
  usedOf,
} from "../../model/quotas";
import type { Namespace, NamespaceOrder, QuotaAlert, Quotas } from "../../model/types";
import { useAccess } from "../../state/access";
import {
  useNamespaceCount,
  useNamespaceDefaults,
  useNamespaceList,
  useQuotaAlerts,
  useSetNamespaceDefaults,
} from "../../state/namespaces";
import { scopedPath } from "../../state/scope";
import { Seg } from "../FilterBits";
import { useListQuery, useNamespaceNameValues } from "../search";
import { TableTop } from "../TableTop";
import { AlertChip, NamespacesGate, QuotaMeter, QuotasForm, worstAlert } from "./bits";

const ORDERS: { value: NamespaceOrder; label: string; title: string }[] = [
  { value: "cpu", label: "CPU", title: "The most CPU time this month first" },
  { value: "runs", label: "Runs", title: "The most runs started this month first" },
  { value: "scripts", label: "Scripts", title: "The most distinct scripts first" },
  { value: "name", label: "Name", title: "By name" },
];

/** Which column an order sorts. */
const SORTED: Partial<Record<NamespaceOrder, string>> = {
  cpu: "cpuPerMonth",
  runs: "runsPerMonth",
  scripts: "scripts",
};

const ABOVE = 0.8;

const LEGACY_NAMES = ["prefix"] as const;

function readOrder(params: URLSearchParams): NamespaceOrder {
  const order = params.get("order");
  return ORDERS.some((o) => o.value === order) ? (order as NamespaceOrder) : "cpu";
}

/** A link to a namespace's scope: the first view the caller may open there (its Overview). */
export function namespacePath(name: string): string {
  return scopedPath("/", name);
}

/** The limits that are not meters: the longest TTL and the longest script. */
function otherLimits(quotas: Quotas | undefined): string {
  const parts = (["maxTtl", "maxScriptLines"] as const).map((key) => {
    const limit = limitOf(quotas, key);
    const name = key === "maxTtl" ? "TTL" : "script";
    return limit === null ? `${name}: no limit` : `${name} ${formatQuota(key, limit)}`;
  });
  return parts.join(" · ");
}

/** What a row says under the namespace's name: its alert, its exceptions, or its last activity. */
function RowNote({ ns, alert }: { ns: Namespace; alert: QuotaAlert | null }) {
  if (ns.deletingSince) return <span className="tag">being erased</span>;
  if (alert) {
    return (
      <span className="ns-note">
        <AlertChip alert={alert} />{" "}
        <span className="muted">{shortDate(Date.parse(alert.raisedAt))}</span>
      </span>
    );
  }
  const exceptions = QUOTA_FIELDS.filter((f) => limitOf(ns.exceptions, f.key) !== null);
  if (exceptions.length > 0) {
    return (
      <span className="ns-note muted">
        exceptions: {exceptions.map((f) => f.label.toLowerCase()).join(", ")}
      </span>
    );
  }
  return (
    <span className="ns-note muted">
      active <Ago ms={Date.parse(ns.lastActiveAt)} />
    </span>
  );
}

function DefaultsEditor({ defaults, onDone }: { defaults: Quotas; onDone: () => void }) {
  const save = useSetNamespaceDefaults();
  return (
    <QuotasForm
      label="Default limits"
      intro={
        <>
          The limits of every namespace without an exception. An empty field sets no limit. The
          ceiling, set by the operator, bounds the organization's totals whatever the defaults.
        </>
      }
      initial={defaults}
      blank={() => "no limit"}
      submit="Save the defaults"
      pending={save.pending}
      error={save.error}
      onSubmit={(quotas) => {
        void save.run(quotas).then((done) => done && onDone());
      }}
    />
  );
}

export function NamespacesView() {
  return (
    <NamespacesGate>
      <Namespaces />
    </NamespacesGate>
  );
}

function Namespaces() {
  const access = useAccess();
  const { q, setQ, params, setParams } = useListQuery(legacyNamespacesQuery, LEGACY_NAMES);
  const { prefix, fault } = namespacesSearch(q);
  const names = useNamespaceNameValues();
  const order = readOrder(params);
  const above = params.get("above") === "80";
  const [editing, setEditing] = useState(false);
  const [folded, setFolded] = useState(false);

  const set = (name: string, value: string | null) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (value === null) next.delete(name);
        else next.set(name, value);
        return next;
      },
      { replace: true },
    );

  const query = {
    order,
    limit: 50,
    ...(prefix ? { prefix } : {}),
    ...(above ? { above: ABOVE } : {}),
  };
  const list = useNamespaceList(query);
  const atRisk = useNamespaceCount({ above: ABOVE });
  const defaults = useNamespaceDefaults();
  const alerts = useQuotaAlerts(null);

  const month = list.head?.month ?? defaults.data?.month ?? monthOf(Date.now());
  const rows = list.rows;
  const total = list.head?.total ?? rows.length;
  const riskyCount = atRisk.data === undefined ? null : formatCount(atRisk.data);
  const byNamespace = useMemo(() => {
    const map = new Map<string, QuotaAlert[]>();
    for (const a of alerts.data?.alerts ?? []) {
      if (!a.namespace) continue;
      map.set(a.namespace, [...(map.get(a.namespace) ?? []), a]);
    }
    return map;
  }, [alerts.data]);
  const tenantAlerts = (alerts.data?.alerts ?? []).filter((a) => !a.namespace);
  const sorted = SORTED[order];
  const header = (key: string, title: string): ReactNode => (
    <th key={key} aria-sort={sorted === key ? "descending" : undefined}>
      {title}
      {sorted === key ? " ↓" : ""}
    </th>
  );
  const organization = access.organization?.name ?? "Organization";
  const write = access.can("namespaces:write");

  return (
    <div className="page">
      <PageHeader
        title="Namespaces"
        info={
          <>
            <p className="info-card-head">
              {monthLabel(month)}
              {riskyCount !== null && <> · {riskyCount} at 80% or more of a limit</>}
            </p>
            <p>
              A namespace is one end user of yours: their runs, tokens and use. The ceiling is the
              operator's, on the organization's totals; the defaults are yours, for every namespace,
              with exceptions per namespace. CPU time is never refused: it alerts at 80% and 100%.
            </p>
          </>
        }
        actions={
          write &&
          defaults.data && (
            <button type="button" className="btn" onClick={() => setEditing(true)}>
              <Pencil size={14} aria-hidden="true" /> Edit the defaults
            </button>
          )
        }
      />
      <FormDialog open={editing && defaults.data !== undefined} onOpenChange={setEditing} wide>
        {defaults.data && (
          <DefaultsEditor defaults={defaults.data.defaults} onDone={() => setEditing(false)} />
        )}
      </FormDialog>

      {tenantAlerts.length > 0 && (
        <div className="notice notice-critical" role="status">
          The organization crossed its ceiling:{" "}
          {tenantAlerts.map((a) => (
            <span key={`${a.quota}-${a.threshold}`}>
              <AlertChip alert={a} />{" "}
            </span>
          ))}
        </div>
      )}

      {(alerts.data?.alerts.length ?? 0) > 0 && (
        <section className="panel" aria-label="Alerts this month">
          <div className="panel-head">
            <h2>Alerts in {monthLabel(month)}</h2>
            <span className="end muted">sent to your webhooks as quota.alert</span>
          </div>
          <ul className="alert-list">
            {(alerts.data?.alerts ?? []).slice(0, 12).map((a) => (
              <li key={`${a.namespace ?? "*"}-${a.quota}-${a.threshold}`}>
                <AlertChip alert={a} />
                {a.namespace ? (
                  <Link to={scopedPath("/usage", a.namespace)}>
                    <NamespaceName name={a.namespace} />
                  </Link>
                ) : (
                  <span>{organization}</span>
                )}
                <span className="muted">
                  {QUOTA_NAMES[a.quota]} at {formatQuota(QUOTA_KEYS[a.quota], a.used)} of{" "}
                  {formatQuota(QUOTA_KEYS[a.quota], a.limit)}
                </span>
                <span className="end muted">
                  <Ago ms={Date.parse(a.raisedAt)} />
                  {a.sentAt ? "" : " · not sent yet"}
                </span>
              </li>
            ))}
          </ul>
        </section>
      )}

      <QueryBar
        value={q}
        onSubmit={setQ}
        language={NAMESPACES_LANGUAGE}
        values={names.values}
        onTyping={names.onTyping}
        fault={fault}
        placeholder="The start of a namespace's name: user_7Kd"
      >
        <Seg<NamespaceOrder>
          label="Order"
          value={order}
          options={ORDERS}
          onChange={(v) => set("order", v === "cpu" ? null : v)}
        />
        <button
          type="button"
          className={`btn${above ? " pressed" : ""}`}
          aria-pressed={above}
          onClick={() => set("above", above ? null : "80")}
          title="Only the namespaces that used 80% or more of one of their limits this month"
        >
          At 80% or more{riskyCount !== null ? ` · ${riskyCount}` : ""}
        </button>
      </QueryBar>

      {fault ? (
        <Empty>Fix the search to see the namespaces: the term at fault is underlined above.</Empty>
      ) : list.status === "pending" ? (
        <Loading>Loading the namespaces…</Loading>
      ) : list.status === "error" ? (
        <ProblemView error={list.error} title="The namespaces could not be read" />
      ) : (
        <div className="live-table">
          <TableTop pending={list.pending} onShow={list.showPending} setAtTop={list.setAtTop} />
          <TableScroll stale={list.stale}>
            <table className="grid namespaces-table">
              <thead>
                <tr>
                  <th>Namespace</th>
                  {METERED.map((m) => header(m.key, m.title))}
                </tr>
              </thead>
              <tbody>
                {defaults.data && (
                  <tr className="row-ceiling">
                    <th scope="row">
                      <button
                        type="button"
                        className="group-toggle ns-title"
                        aria-expanded={!folded}
                        aria-label={`${folded ? "Show" : "Hide"} the namespaces of ${organization}`}
                        onClick={() => setFolded(!folded)}
                      >
                        {folded ? <ChevronRight size={14} /> : <ChevronDown size={14} />}
                        {organization}
                      </button>
                      <span className="ns-note muted">
                        operator's ceiling, on the totals
                        {folded
                          ? ` · ${formatCount(total)} namespace${total === 1 ? "" : "s"}`
                          : ""}
                      </span>
                    </th>
                    {METERED.map((m) => (
                      <td key={m.key}>
                        <QuotaMeter
                          spec={m}
                          used={usedOf(defaults.data.usage, m.key)}
                          limit={limitOf(defaults.data.ceiling, m.key)}
                        />
                      </td>
                    ))}
                  </tr>
                )}
                {defaults.data && !folded && (
                  <tr className="row-defaults child-row">
                    <th scope="row">
                      <span className="ns-title">Defaults</span>
                      <span className="ns-note muted">{otherLimits(defaults.data.defaults)}</span>
                    </th>
                    {METERED.map((m) => {
                      const limit = limitOf(defaults.data.defaults, m.key);
                      return (
                        <td key={m.key} className="num">
                          {limit === null ? (
                            <span className="muted">no limit</span>
                          ) : (
                            formatQuota(m.key, limit)
                          )}
                        </td>
                      );
                    })}
                  </tr>
                )}
                {(folded ? [] : rows).map((ns) => (
                  <tr key={ns.name} className={`child-row${ns.deletingSince ? " dimmed" : ""}`}>
                    <th scope="row">
                      <Link to={namespacePath(ns.name)} className="ns-title">
                        <NamespaceName name={ns.name} displayName={ns.displayName ?? null} />
                      </Link>
                      <RowNote ns={ns} alert={worstAlert(byNamespace.get(ns.name) ?? [])} />
                    </th>
                    {METERED.map((m) => (
                      <td key={m.key}>
                        <QuotaMeter
                          spec={m}
                          used={usedOf(ns.usage, m.key)}
                          limit={limitOf(ns.limits, m.key)}
                        />
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
            {rows.length === 0 && (
              <Empty>
                {prefix
                  ? `No namespace starts with ${prefix}${above ? " and reached 80% of a limit" : ""}.`
                  : above
                    ? "No namespace reached 80% of a limit this month."
                    : "No namespace yet: one is created with the first run or token of an end user."}
              </Empty>
            )}
            {rows.length > 0 && (
              <div className="list-foot">
                <span className="num">
                  {rows.length < total ? `${formatCount(rows.length)} of ` : ""}
                  {formatCount(total)} namespace{total === 1 ? "" : "s"}
                </span>
                {list.hasMore && (
                  <button
                    type="button"
                    className="btn small"
                    disabled={list.loadingMore}
                    onClick={list.loadMore}
                  >
                    {list.loadingMore ? "Loading…" : "Load more"}
                  </button>
                )}
              </div>
            )}
          </TableScroll>
        </div>
      )}
    </div>
  );
}
