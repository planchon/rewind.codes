// A namespace's page: one end user. Its CPU time added up over the month against its quota, its
// use of every limit, the alerts of the month, its tokens in use, its exceptions to the defaults,
// links to its runs and its scripts; and what the caller's permissions allow, each in a dialog:
// change its limits (`namespaces:write`), revoke its tokens (`tokens:write`), erase it
// (`namespaces:write`, confirmed by typing its name).
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ChevronLeft, ChevronRight, Pencil, Trash2 } from "lucide-react";
import { useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { NAMESPACES, useNamespaceDefaults, useNamespaceDetail } from "../../api/namespaces";
import type { NamespaceDetail, Quotas, RevokedTokens } from "../../api/types";
import { useAccess } from "../../app/access";
import { useApi } from "../../app/session";
import { Ago, CopyButton, Loading, ProblemView, Time, useNow } from "../../components/bits";
import { CpuChart } from "../../components/CpuChart";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import { PageHeader } from "../../components/PageHeader";
import { formatCount } from "../../model/format";
import { fieldTerm, queryLink } from "../../model/query";
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
import { AlertChip, NamespacesGate, QuotaMeter, QuotasForm } from "./bits";

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
  onDone,
}: {
  detail: NamespaceDetail;
  defaults: Quotas | null;
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
      label={`Limits of ${name}`}
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

/** Erasing an end user, in a dialog, confirmed by typing its name. */
function Erase({ name, onErased }: { name: string; onErased: () => void }) {
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
      title={`Erase ${name}`}
      description={
        <>
          The end user leaves: their tokens are revoked and their active runs cancelled, then their
          runs, journals, credentials and use are erased. This cannot be undone, and the namespace
          takes no run or token meanwhile. The organization's totals keep what it used.
        </>
      }
      submit={`Erase ${name}`}
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
  const deleting = ns.deletingSince ?? null;
  const cpuLimit = limitOf(ns.limits, "cpuPerMonth");
  const points = cumulativeCpu(d.days, d.month, now);
  const crossed = cpuLimit ? crossingDay(points, cpuLimit, 100) : null;
  const canWrite = access.can("namespaces:write") && !deleting;
  const canRevoke = access.can("tokens:write") && !deleting;
  const runs = queryLink("/observability", fieldTerm("namespace", ns.name));
  const scripts = queryLink("/scripts", fieldTerm("namespace", ns.name));

  return (
    <div className="page">
      <PageHeader
        crumbs={<Link to="/namespaces">Namespaces</Link>}
        title={
          <span className="copyable">
            <span className="mono">{ns.name}</span>
            <CopyButton text={ns.name} label="Copy the namespace" />
          </span>
        }
        actions={
          <>
            {canWrite && (
              <button type="button" className="btn" onClick={() => setEditing(true)}>
                <Pencil size={12} /> Edit its limits
              </button>
            )}
            {canRevoke && (
              <ConfirmButton
                confirm={`Revoke every token of ${ns.name}?`}
                onConfirm={() => revoke.mutate()}
                pending={revoke.isPending}
                title="Its opaque tokens, and its signed tokens issued before now"
              >
                Revoke its tokens
              </ConfirmButton>
            )}
            {canWrite && (
              <button type="button" className="btn" onClick={() => setErasing(true)}>
                <Trash2 size={12} /> Erase…
              </button>
            )}
          </>
        }
      />
      <p className="page-intro">
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
        {" · "}
        <Link to={runs} title="Its runs, in Observability">
          See its runs
        </Link>
        {" · "}
        <Link to={scripts} title="The scripts its runs ran">
          See its scripts
        </Link>
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
          its active runs cancelled; its runs, credentials and use are erased in the background.
        </div>
      )}
      <FormDialog open={erasing && canWrite} onOpenChange={setErasing}>
        <Erase
          name={ns.name}
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

      <div className="ns-grid">
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
                name={ns.name}
              />
            )}
          </div>
        </section>

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
  );
}
