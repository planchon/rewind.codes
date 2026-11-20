// A namespace's page: one end user, shown by its display name when it has one (its id on hover).
// Over a month: how its runs went (started, completed, failed, waiting, durations, by day, what
// fails most, the tools they call), its CPU time added up against its quota, its use of every
// limit, the alerts of the month, its tokens in use, its exceptions to the defaults; then its runs
// of the month, the scripts its runs ran, and its secrets (`secrets:read`). What the caller's
// permissions allow, each in a dialog: rename it and change its limits (`namespaces:write`), revoke
// its tokens (`tokens:write`), write and delete its secrets (`secrets:write`), erase it
// (`namespaces:write`, confirmed by typing its id).
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  ChevronLeft,
  ChevronRight,
  KeyRound,
  PenLine,
  SlidersHorizontal,
  Trash2,
} from "lucide-react";
import { useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";
import { NAMESPACES, useNamespaceDefaults, useNamespaceDetail } from "../../api/namespaces";
import { useFailures, useRunStats, useRuns, useScripts } from "../../api/queries";
import type { NamespaceDetail, Quotas, RevokedTokens, RunStatus } from "../../api/types";
import { useAccess } from "../../app/access";
import { useApi } from "../../app/session";
import { Ago, CopyButton, Empty, Loading, ProblemView, Time, useNow } from "../../components/bits";
import { Meter, StackedBars, Tile } from "../../components/Charts";
import { CpuChart } from "../../components/CpuChart";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import { NamespaceName } from "../../components/NamespaceName";
import { PageHeader } from "../../components/PageHeader";
import { isCancellation } from "../../model/ending";
import { formatCount, formatDuration, formatPercent } from "../../model/format";
import { fieldTerm, joinQuery, labelTerm, queryLink, queryTime } from "../../model/query";
import {
  crossingDay,
  cumulativeCpu,
  dayLabel,
  daysInMonth,
  formatCpu,
  formatQuota,
  isMonth,
  limitOf,
  METERED,
  monthBounds,
  monthLabel,
  monthOf,
  QUOTA_FIELDS,
  QUOTA_KEYS,
  QUOTA_NAMES,
  quotaText,
  shiftMonth,
  usedOf,
} from "../../model/quotas";
import { ConfirmButton } from "../org/bits";
import { RunTable } from "../RunTable";
import { ScriptTable } from "../ScriptsView";
import { SecretList } from "../secrets/SecretList";
import { AlertChip, NamespacesGate, QuotaMeter, QuotasForm } from "./bits";

const POLL = 30_000;
const DAY_MS = 86_400_000;
/** The longest a display name may be, in characters, as the API checks it. */
const DISPLAY_NAME_MAX = 128;

export function NamespaceView() {
  return (
    <NamespacesGate>
      <NamespacePage />
    </NamespacesGate>
  );
}

/** Its exceptions to the defaults, each with the default it replaces. */
function Exceptions({ detail, defaults }: { detail: NamespaceDetail; defaults: Quotas | null }) {
  const set = QUOTA_FIELDS.filter((f) => limitOf(detail.namespace.exceptions, f.key) !== null);
  if (set.length === 0) return <span className="muted">none: the defaults apply</span>;
  return (
    <ul className="plain-list">
      {set.map((f) => {
        const value = limitOf(detail.namespace.exceptions, f.key) ?? 0;
        const fallback = defaults ? limitOf(defaults, f.key) : null;
        return (
          <li key={f.key}>
            {f.label}: <strong className="num">{formatQuota(f.key, value)}</strong>
            {defaults && (
              <span className="muted">
                {" "}
                (default {fallback === null ? "no limit" : formatQuota(f.key, fallback)})
              </span>
            )}
          </li>
        );
      })}
    </ul>
  );
}

/** The change of a namespace's exceptions. */
function ExceptionsEditor({
  detail,
  defaults,
  label,
  onDone,
}: {
  detail: NamespaceDetail;
  defaults: Quotas | null;
  label: string;
  onDone: () => void;
}) {
  const api = useApi();
  const queryClient = useQueryClient();
  const name = detail.namespace.name;
  const save = useMutation({
    mutationFn: (exceptions: Quotas) => api.putNamespace(name, { exceptions }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: NAMESPACES });
      onDone();
    },
  });
  return (
    <QuotasForm
      label={`Limits of ${label}`}
      intro={
        <>
          Exceptions to the defaults, for this end user only: a paying one, or a blocked one (active
          runs 0). An empty field follows the default.
        </>
      }
      initial={detail.namespace.exceptions}
      blank={(key) => {
        const fallback = defaults ? limitOf(defaults, key) : null;
        return fallback === null
          ? "default: no limit"
          : `default: ${quotaText(defaults ?? {}, key)}`;
      }}
      submit="Save the limits"
      pending={save.isPending}
      error={save.error}
      onSubmit={(quotas) => save.mutate(quotas)}
    />
  );
}

/** The display name of the namespace, in a dialog: its id never changes. */
function Rename({
  name,
  displayName,
  onDone,
}: {
  name: string;
  displayName: string | null;
  onDone: () => void;
}) {
  const api = useApi();
  const queryClient = useQueryClient();
  const [typed, setTyped] = useState(displayName ?? "");
  const next = typed.trim();
  const tooLong = [...next].length > DISPLAY_NAME_MAX;
  const save = useMutation({
    mutationFn: () => api.patchNamespace(name, { displayName: next === "" ? null : next }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: NAMESPACES });
      onDone();
    },
  });
  return (
    <DialogForm
      title={displayName ? `Rename ${displayName}` : `Name ${name}`}
      description={
        <>
          The name every view shows for this end user. Its id, <span className="mono">{name}</span>,
          stays what its tokens, runs, links and webhooks use. Empty, the views show the id.
        </>
      }
      submit={next === "" && displayName ? "Remove the name" : "Save the name"}
      pending={save.isPending}
      disabled={tooLong || next === (displayName ?? "")}
      error={save.error}
      errorTitle="Could not rename it"
      onSubmit={() => {
        if (!tooLong) save.mutate();
      }}
    >
      <label className="field col">
        <span className="field-label">Display name</span>
        <input
          className="input"
          value={typed}
          placeholder={name}
          maxLength={DISPLAY_NAME_MAX * 2}
          autoComplete="off"
          // biome-ignore lint/a11y/noAutofocus: the dialog opens to type a name
          autoFocus
          onChange={(e) => setTyped(e.target.value)}
        />
        {tooLong && (
          <span className="field-hint tone-critical">At most {DISPLAY_NAME_MAX} characters.</span>
        )}
      </label>
    </DialogForm>
  );
}

/** Erasing an end user, in a dialog, confirmed by typing its id. */
function Erase({ name, label, onErased }: { name: string; label: string; onErased: () => void }) {
  const api = useApi();
  const queryClient = useQueryClient();
  const [typed, setTyped] = useState("");
  const erase = useMutation({
    mutationFn: () => api.deleteNamespace(name),
    onSuccess: async () => {
      onErased();
      await queryClient.invalidateQueries({ queryKey: NAMESPACES });
    },
  });
  return (
    <DialogForm
      title={`Erase ${label}`}
      description={
        <>
          The end user leaves: their tokens are revoked and their active runs cancelled, then their
          runs, journals, secrets and use are erased. This cannot be undone, and the namespace takes
          no run or token meanwhile. The organization's totals keep what it used.
        </>
      }
      submit={`Erase ${label}`}
      pending={erase.isPending}
      pendingLabel="Erasing…"
      disabled={typed !== name}
      danger
      error={erase.error}
      errorTitle="Could not erase it"
      onSubmit={() => {
        if (typed === name) erase.mutate();
      }}
    >
      <label className="field col">
        <span className="field-label">
          Type <span className="mono">{name}</span> to confirm
        </span>
        <input
          className="input mono"
          value={typed}
          spellCheck={false}
          autoComplete="off"
          onChange={(e) => setTyped(e.target.value)}
        />
      </label>
    </DialogForm>
  );
}

/**
 * How the namespace's runs of the month went: figures, the runs started each day by outcome, what
 * fails most and the tools they call. Every figure, bar and row opens the runs behind it.
 */
function useMonthRuns(name: string, month: string, now: number) {
  const [from, end] = monthBounds(month);
  const until = Math.min(end, now);
  const scope = fieldTerm("namespace", name);
  const started = `started:${queryTime(from)}..${queryTime(until)}`;
  const range = { since: new Date(from).toISOString(), until: new Date(until).toISOString() };
  const key = `${name}|${month}`;
  const stats = useRunStats(
    `namespace|${key}`,
    () => ({ q: scope, ...range, bucket: "1d", top: 6 }),
    POLL,
  );
  const waiting = useRunStats(
    `namespace-waiting|${name}`,
    () => ({ q: joinQuery(scope, "waiting:*"), top: 1 }),
    POLL,
  );
  const failures = useFailures(
    `namespace-failures|${key}`,
    () => ({ q: scope, ...range, limit: 5 }),
    POLL,
  );
  return { scope, started, from, until, stats, waiting, failures };
}

type MonthRuns = ReturnType<typeof useMonthRuns>;

function RunFigures({ runs, month }: { runs: MonthRuns; month: string }) {
  const { scope, started } = runs;
  if (runs.stats.isPending) return <Loading>Loading its runs…</Loading>;
  if (runs.stats.isError)
    return <ProblemView error={runs.stats.error} title="Its runs could not be counted" />;
  const s = runs.stats.data;
  const ended =
    s.statuses.completed + s.statuses.failed + s.statuses.cancelled + s.statuses.aborted;
  const failed = s.statuses.failed + s.statuses.aborted;
  const waitingNow = runs.waiting.data;
  const durations = s.durations;
  const link = (...terms: string[]) => queryLink("/observability", joinQuery(scope, ...terms));
  return (
    <section className="tiles" aria-label={`Runs in ${monthLabel(month)}`}>
      <Tile
        to={link(started)}
        label="Runs started"
        value={formatCount(s.runs)}
        context={`in ${monthLabel(month)}`}
      />
      <Tile
        to={link(started, "status:completed")}
        label="Completed"
        value={formatPercent(s.statuses.completed, ended)}
        context={`of the ${formatCount(ended)} runs that ended`}
        tone="good"
      />
      <Tile
        to={queryLink("/observability/failures", joinQuery(scope, started))}
        label="Failed"
        value={formatCount(s.statuses.failed)}
        context={`+ ${formatCount(s.statuses.aborted)} aborted, ${formatCount(s.statuses.cancelled)} cancelled`}
        tone={failed > 0 ? "critical" : undefined}
      />
      <Tile
        to={queryLink("/observability/waiting", scope)}
        label="Waiting now"
        value={waitingNow ? formatCount(waitingNow.runs) : "–"}
        context={
          waitingNow
            ? `${formatCount(waitingNow.waiting.signal)} on a signal, ${formatCount(waitingNow.waiting.approval)} on an approval`
            : " "
        }
        tone="info"
      />
      <Tile
        to={durations ? link(started, `duration:>=${durations.p95}`) : link(started)}
        label="Duration p95"
        value={durations ? formatDuration(durations.p95) : "–"}
        context={
          durations
            ? `p50 ${formatDuration(durations.p50)}, longest ${formatDuration(durations.max)}`
            : "no run ended"
        }
      />
    </section>
  );
}

function RunsByDay({ runs, month }: { runs: MonthRuns; month: string }) {
  const s = runs.stats.data;
  const buckets = (s?.series ?? []).map((b) => {
    const from = Date.parse(b.at);
    return { from, to: from + DAY_MS, counts: b.statuses };
  });
  const day = (ms: number) => String(new Date(ms).getUTCDate());
  return (
    <section className="panel" aria-label="Runs started by day">
      <div className="panel-head">
        <h2>Runs started each day, by outcome</h2>
        <span className="end muted">UTC days</span>
      </div>
      <div className="panel-body">
        {!s ? (
          <Loading />
        ) : s.runs === 0 || buckets.length === 0 ? (
          <Empty>No run started in {monthLabel(month)}.</Empty>
        ) : (
          <StackedBars
            buckets={buckets}
            title="Runs started"
            tick={day}
            bucketLabel={(b) => dayLabel(month, new Date(b.from).getUTCDate())}
            href={(b, status: RunStatus) =>
              queryLink(
                "/observability",
                joinQuery(
                  runs.scope,
                  `status:${status}`,
                  `started:${queryTime(Math.max(b.from, runs.from))}..${queryTime(Math.min(b.to, runs.until))}`,
                ),
              )
            }
          />
        )}
      </div>
    </section>
  );
}

function FailsMost({ runs, month }: { runs: MonthRuns; month: string }) {
  const groups = runs.failures.data?.groups ?? [];
  const ended = `ended:${queryTime(runs.from)}..${queryTime(runs.until)}`;
  return (
    <section className="panel" aria-label="What fails most">
      <div className="panel-head">
        <h2>What fails most</h2>
        <span className="end">
          <Link to={queryLink("/observability/failures", joinQuery(runs.scope, runs.started))}>
            Every failure →
          </Link>
        </span>
      </div>
      {runs.failures.isPending ? (
        <Loading />
      ) : groups.length === 0 ? (
        <Empty>Nothing failed in {monthLabel(month)}.</Empty>
      ) : (
        <ul className="rank-list">
          {groups.map((g) => (
            <li key={g.fingerprint}>
              <Link
                to={queryLink(
                  "/observability",
                  joinQuery(runs.scope, `fingerprint:${g.fingerprint}`, ended),
                )}
                title={g.signature}
              >
                <span className="rank-name mono">{g.signature}</span>
                <Meter
                  value={g.runs}
                  max={groups[0]?.runs ?? 1}
                  tone={isCancellation(g.error) ? "neutral" : "critical"}
                />
                <span className="num">{formatCount(g.runs)}</span>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function ToolsUsed({ runs, month }: { runs: MonthRuns; month: string }) {
  const tools = runs.stats.data?.facets.tools ?? [];
  return (
    <section className="panel" aria-label="Tools its runs call">
      <div className="panel-head">
        <h2>Tools its runs call</h2>
        <span className="end muted">runs that called each</span>
      </div>
      {!runs.stats.data ? (
        <Loading />
      ) : tools.length === 0 ? (
        <Empty>No run called a tool in {monthLabel(month)}.</Empty>
      ) : (
        <ul className="rank-list">
          {tools.map((t) => (
            <li key={t.value}>
              <Link
                to={queryLink(
                  "/observability",
                  joinQuery(runs.scope, runs.started, fieldTerm("tool", t.value)),
                )}
                title={`${t.runs} runs called ${t.value}`}
              >
                <span className="rank-name mono">{t.value}</span>
                <Meter value={t.runs} max={tools[0]?.runs ?? 1} tone="info" />
                <span className="num">{formatCount(t.runs)}</span>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

/** Its runs started in the month, the newest first. */
function MonthRunList({ runs, month }: { runs: MonthRuns; month: string }) {
  const navigate = useNavigate();
  const q = joinQuery(runs.scope, runs.started);
  const list = useRuns(`namespace|${q}`, () => ({ q, limit: 20 }), POLL);
  const open = (...terms: string[]) =>
    void navigate(queryLink("/observability", joinQuery(q, ...terms)));
  return (
    <section className="ns-section" aria-label="Its runs">
      <header className="ns-section-head">
        <h2>Runs in {monthLabel(month)}</h2>
        <Link to={queryLink("/observability", q)}>Open in Observability →</Link>
      </header>
      <RunTable
        query={list}
        onPickLabel={(pair) => {
          const at = pair.indexOf(":");
          open(labelTerm(pair.slice(0, at), pair.slice(at + 1)));
        }}
        onPickScript={(digest) => open(`script:${digest}`)}
        empty={`No run started in ${monthLabel(month)}.`}
      />
    </section>
  );
}

/** The scripts its runs ran, the most recently run first. */
function ScriptList({ name, scope, count }: { name: string; scope: string; count: number }) {
  const navigate = useNavigate();
  const scripts = useScripts(`namespace|${name}`, { limit: 10 }, name);
  const open = (term: string) => void navigate(queryLink("/scripts", joinQuery(scope, term)));
  return (
    <section className="ns-section" aria-label="Its scripts">
      <header className="ns-section-head">
        <h2>
          Scripts <span className="muted num">{formatCount(count)}</span>
        </h2>
        <Link to={queryLink("/scripts", scope)}>Open in Scripts →</Link>
      </header>
      <ScriptTable
        query={scripts}
        scope={scope}
        onPickLabel={(pair) => {
          const at = pair.indexOf(":");
          open(labelTerm(pair.slice(0, at), pair.slice(at + 1)));
        }}
        onPickTool={(tool) => open(fieldTerm("tool", tool))}
        empty="No script ran in this namespace yet."
      />
    </section>
  );
}

/** The end user's own secrets, which its runs attach as `ns/<name>`. */
function NamespaceSecrets({ name }: { name: string }) {
  const access = useAccess();
  return (
    <section className="ns-section" aria-label="Its secrets">
      <header className="ns-section-head">
        <h2>Secrets</h2>
      </header>
      <SecretList
        namespace={name}
        intro={
          <>
            This end user's own values, such as their GitHub token: only the runs of this namespace
            attach them, as <code>ns/&lt;name&gt;</code>, and nothing falls back on the
            organization's.
            {access.can("secrets:write") && access.limited
              ? " Your grant is limited to labels: it reads them, and never writes one."
              : ""}
          </>
        }
      />
    </section>
  );
}

function NamespacePage() {
  const { name = "" } = useParams();
  const [params, setParams] = useSearchParams();
  const access = useAccess();
  const api = useApi();
  const queryClient = useQueryClient();
  const now = useNow(60_000);
  const current = monthOf(now);
  const asked = params.get("month");
  const month = asked && isMonth(asked) && asked <= current ? asked : current;
  const detail = useNamespaceDetail(name, month === current ? null : month);
  const defaults = useNamespaceDefaults();
  const runs = useMonthRuns(name, month, now);
  const [renaming, setRenaming] = useState(false);
  const [editing, setEditing] = useState(false);
  const [erasing, setErasing] = useState(false);
  const [erased, setErased] = useState(false);
  const [revoked, setRevoked] = useState<RevokedTokens | null>(null);
  const revoke = useMutation({
    mutationFn: () => api.revokeNamespaceTokens(name),
    onMutate: () => setRevoked(null),
    onSuccess: async (answer) => {
      setRevoked(answer);
      await queryClient.invalidateQueries({ queryKey: NAMESPACES });
    },
  });
  const toMonth = (next: string) =>
    setParams((p) => {
      const out = new URLSearchParams(p);
      if (next === current) out.delete("month");
      else out.set("month", next);
      return out;
    });

  if (detail.isPending)
    return (
      <div className="page">
        <Loading>Loading the namespace…</Loading>
      </div>
    );
  if (detail.isError)
    return (
      <div className="page">
        <ProblemView error={detail.error} title={`The namespace ${name} could not be read`} />
        <p>
          <Link to="/namespaces">Every namespace</Link>
        </p>
      </div>
    );

  const d = detail.data;
  const ns = d.namespace;
  const displayName = ns.displayName ?? null;
  const label = displayName ?? ns.name;
  const deleting = ns.deletingSince ?? null;
  const cpuLimit = limitOf(ns.limits, "cpuPerMonth");
  const points = cumulativeCpu(d.days, d.month, now);
  const crossed = cpuLimit ? crossingDay(points, cpuLimit, 100) : null;
  const canWrite = access.can("namespaces:write") && !deleting;
  const canRevoke = access.can("tokens:write") && !deleting;

  return (
    <div className="page">
      <PageHeader
        crumbs={<Link to="/namespaces">Namespaces</Link>}
        title={
          <span className="copyable">
            <NamespaceName name={ns.name} displayName={displayName} />
            <CopyButton text={ns.name} label="Copy the namespace's id" />
          </span>
        }
        actions={
          <>
            {canWrite && (
              <button type="button" className="btn" onClick={() => setRenaming(true)}>
                <PenLine size={14} aria-hidden="true" /> Rename
              </button>
            )}
            {canWrite && (
              <button type="button" className="btn" onClick={() => setEditing(true)}>
                <SlidersHorizontal size={14} aria-hidden="true" /> Edit its limits
              </button>
            )}
            {canRevoke && (
              <ConfirmButton
                confirm={`Revoke every token of ${label}?`}
                description="Its opaque tokens, and the signed tokens issued before now, are refused from their next request. The end user's agents need new tokens."
                onConfirm={() => revoke.mutate()}
                pending={revoke.isPending}
                title="Its opaque tokens, and its signed tokens issued before now"
                icon={<KeyRound size={14} aria-hidden="true" />}
                small={false}
              >
                Revoke its tokens
              </ConfirmButton>
            )}
            {canWrite && (
              <button type="button" className="btn" onClick={() => setErasing(true)}>
                <Trash2 size={14} aria-hidden="true" /> Erase…
              </button>
            )}
          </>
        }
      />
      <p className="page-intro">
        {displayName && (
          <>
            <span className="muted">id</span> <span className="mono">{ns.name}</span>
            {" · "}
          </>
        )}
        {deleting ? (
          <span className="tag critical">
            being erased since <Ago ms={Date.parse(deleting)} />
          </span>
        ) : (
          <>
            Created <Time ms={Date.parse(ns.createdAt)} /> · active{" "}
            <Ago ms={Date.parse(ns.lastActiveAt)} />
          </>
        )}
      </p>

      {revoked && (
        <div className="notice" role="status">
          {formatCount(revoked.revoked)} token{revoked.revoked === 1 ? "" : "s"} revoked; the signed
          tokens issued before <Time ms={Date.parse(revoked.revokedBefore)} /> are refused.
        </div>
      )}
      {revoke.error ? <ErrorNote error={revoke.error} title="Could not revoke its tokens" /> : null}
      {erased && (
        <div className="notice" role="status">
          The erasure of <span className="mono">{ns.name}</span> started: its tokens are revoked and
          its active runs cancelled; its runs, secrets and use are erased in the background.
        </div>
      )}
      <FormDialog open={renaming && canWrite} onOpenChange={setRenaming}>
        <Rename name={ns.name} displayName={displayName} onDone={() => setRenaming(false)} />
      </FormDialog>
      <FormDialog open={erasing && canWrite} onOpenChange={setErasing}>
        <Erase
          name={ns.name}
          label={label}
          onErased={() => {
            setErasing(false);
            setErased(true);
          }}
        />
      </FormDialog>
      <FormDialog open={editing && canWrite} onOpenChange={setEditing} wide>
        <ExceptionsEditor
          detail={d}
          defaults={defaults.data?.defaults ?? null}
          label={label}
          onDone={() => setEditing(false)}
        />
      </FormDialog>

      <nav className="month-nav" aria-label="Month">
        <button
          type="button"
          className="icon-btn"
          aria-label="The month before"
          onClick={() => toMonth(shiftMonth(month, -1))}
        >
          <ChevronLeft size={14} />
        </button>
        <span className="month-name">{monthLabel(d.month)}</span>
        <button
          type="button"
          className="icon-btn"
          aria-label="The month after"
          disabled={month >= current}
          onClick={() => toMonth(shiftMonth(month, 1))}
        >
          <ChevronRight size={14} />
        </button>
      </nav>

      <RunFigures runs={runs} month={month} />

      <div className="ns-columns">
        <div className="ns-column">
          <section className="panel" aria-label="CPU time of the month">
            <div className="panel-head">
              <h2>Cumulative CPU in {monthLabel(d.month)}</h2>
              <span className="end muted">
                {cpuLimit === null
                  ? "no quota"
                  : `quota ${formatCpu(cpuLimit)}${crossed ? `, reached on ${dayLabel(d.month, crossed.day)}` : ""}`}
              </span>
            </div>
            <div className="panel-body">
              {points.length === 0 ? (
                <p className="muted">This month has not started.</p>
              ) : (
                <CpuChart
                  points={points}
                  month={d.month}
                  days={daysInMonth(d.month)}
                  limit={cpuLimit}
                  name={label}
                />
              )}
            </div>
          </section>

          <RunsByDay runs={runs} month={month} />

          <section className="panel" aria-label="Alerts">
            <div className="panel-head">
              <h2>Alerts in {monthLabel(d.month)}</h2>
            </div>
            <div className="panel-body">
              {d.alerts.length === 0 ? (
                <p className="muted">No limit crossed 80% this month.</p>
              ) : (
                <ul className="alert-list">
                  {d.alerts.map((a) => (
                    <li key={`${a.quota}-${a.threshold}`}>
                      <AlertChip alert={a} />
                      <span>
                        {QUOTA_NAMES[a.quota]} at {formatQuota(QUOTA_KEYS[a.quota], a.used)} of{" "}
                        {formatQuota(QUOTA_KEYS[a.quota], a.limit)}
                      </span>
                      <span className="end muted">
                        <Time ms={Date.parse(a.raisedAt)} />
                        {a.sentAt ? " · webhook sent" : " · not sent yet"}
                      </span>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </section>
        </div>

        <div className="ns-column">
          <section className="panel" aria-label="Limits and use">
            <div className="panel-head">
              <h2>Limits and use</h2>
            </div>
            <div className="panel-body">
              <dl className="kv quota-kv">
                {METERED.map((m) => (
                  <div key={m.key}>
                    <dt>{m.title}</dt>
                    <dd>
                      <QuotaMeter
                        spec={m}
                        used={usedOf(ns.usage, m.key)}
                        limit={limitOf(ns.limits, m.key)}
                      />
                    </dd>
                  </div>
                ))}
                {(["maxTtl", "maxScriptLines"] as const).map((key) => {
                  const limit = limitOf(ns.limits, key);
                  return (
                    <div key={key}>
                      <dt>{key === "maxTtl" ? "Longest TTL" : "Longest script"}</dt>
                      <dd className="num">
                        {limit === null ? (
                          <span className="muted">no limit</span>
                        ) : (
                          formatQuota(key, limit)
                        )}
                      </dd>
                    </div>
                  );
                })}
              </dl>
            </div>
          </section>

          <FailsMost runs={runs} month={month} />
          <ToolsUsed runs={runs} month={month} />

          <section className="panel" aria-label="Access">
            <div className="panel-head">
              <h2>Tokens and exceptions</h2>
            </div>
            <div className="panel-body">
              <dl className="kv">
                <div>
                  <dt>Active tokens</dt>
                  <dd className="num" data-testid="tokens">
                    {formatCount(d.tokens)}
                  </dd>
                </div>
                {d.tokensRevokedBefore && (
                  <div>
                    <dt>Signed tokens</dt>
                    <dd>
                      refused when issued before <Time ms={Date.parse(d.tokensRevokedBefore)} />
                    </dd>
                  </div>
                )}
                <div>
                  <dt>Exceptions</dt>
                  <dd>
                    <Exceptions detail={d} defaults={defaults.data?.defaults ?? null} />
                  </dd>
                </div>
              </dl>
            </div>
          </section>
        </div>
      </div>

      <MonthRunList runs={runs} month={month} />
      <ScriptList name={ns.name} scope={runs.scope} count={ns.usage.scripts} />
      {access.can("secrets:read") && <NamespaceSecrets name={ns.name} />}
    </div>
  );
}
