// A namespace's Usage, the last view of its scope (`/ns/<name>/usage`; archi/05, "Namespaces"): the
// end user the picker shows. Over a month: the alerts of the month first, when it has any; its CPU
// time added up against its quota, its use of every limit, its tokens in use, its exceptions to
// the defaults. How its runs went is its Overview; its runs, scripts and secrets, its other views.
// What the caller's permissions allow, each in a dialog: rename it and change its limits
// (`namespaces:write`), revoke its tokens (`tokens:write`), erase it (`namespaces:write`,
// confirmed by typing its id). Each change of the namespace shows on the page at once, and is
// undone if the server refuses it, its message in the dialog (`state/namespaces.ts`).
import {
  ChevronLeft,
  ChevronRight,
  KeyRound,
  PenLine,
  SlidersHorizontal,
  Trash2,
} from "lucide-react";
import { useState } from "react";
import { Link, useSearchParams } from "react-router";
import { Ago, Loading, ProblemView, Time, useNow } from "../../components/bits";
import { CpuChart } from "../../components/CpuChart";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import { PageHeader } from "../../components/PageHeader";
import { formatCount } from "../../model/format";
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
  quotaText,
  shiftMonth,
  usedOf,
} from "../../model/quotas";
import type { NamespaceDetail, Quotas } from "../../model/types";
import { useAccess } from "../../state/access";
import type { Action } from "../../state/actions";
import {
  useEraseNamespace,
  useNamespaceDefaults,
  useNamespaceDetail,
  useRenameNamespace,
  useRevokeNamespaceTokens,
  useSetNamespaceLimits,
} from "../../state/namespaces";
import { useScopeNamespace } from "../../state/scope";
import { ConfirmButton } from "../org/bits";
import { NamespaceAlerts, NamespacesGate, QuotaMeter, QuotasForm } from "./bits";

/** The longest a display name may be, in characters, as the API checks it. */
const DISPLAY_NAME_MAX = 128;

export function NamespaceView() {
  const name = useScopeNamespace() ?? "";
  return (
    <NamespacesGate>
      {/* Another namespace is another page: it never shows the one before while it loads. */}
      <NamespacePage key={name} name={name} />
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
  const save = useSetNamespaceLimits(detail.namespace.name);
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
      pending={save.pending}
      error={save.error}
      onSubmit={(quotas) => {
        void save.run(quotas).then((done) => done && onDone());
      }}
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
  const [typed, setTyped] = useState(displayName ?? "");
  // The name as it was when the dialog opened: the page shows the new one at once.
  const [was] = useState(displayName);
  const next = typed.trim();
  const tooLong = [...next].length > DISPLAY_NAME_MAX;
  const save = useRenameNamespace(name);
  return (
    <DialogForm
      title={was ? `Rename ${was}` : `Name ${name}`}
      description={
        <>
          The name every view shows for this end user. Its id, <span className="mono">{name}</span>,
          stays what its tokens, runs, links and webhooks use. Empty, the views show the id.
        </>
      }
      submit={next === "" && was ? "Remove the name" : "Save the name"}
      pending={save.pending}
      disabled={tooLong || next === (was ?? "")}
      error={save.error}
      errorTitle="Could not rename it"
      onSubmit={() => {
        if (!tooLong) void save.run(next === "" ? null : next).then((done) => done && onDone());
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

/**
 * Erasing an end user, in a dialog, confirmed by typing its id. The page holds the action: it shows
 * the namespace being erased at once, and the dialog stays until the server's answer.
 */
function Erase({
  name,
  label,
  erase,
  onErased,
}: {
  name: string;
  label: string;
  erase: Action<void, void>;
  onErased: () => void;
}) {
  const [typed, setTyped] = useState("");
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
      pending={erase.pending}
      pendingLabel="Erasing…"
      disabled={typed !== name}
      danger
      error={erase.error}
      errorTitle="Could not erase it"
      onSubmit={() => {
        if (typed === name) void erase.run().then((done) => done && onErased());
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

function NamespacePage({ name }: { name: string }) {
  const [params, setParams] = useSearchParams();
  const access = useAccess();
  const now = useNow(60_000);
  const current = monthOf(now);
  const asked = params.get("month");
  const month = asked && isMonth(asked) && asked <= current ? asked : current;
  const detail = useNamespaceDetail(name, month === current ? null : month);
  const defaults = useNamespaceDefaults();
  const [renaming, setRenaming] = useState(false);
  const [editing, setEditing] = useState(false);
  const [erasing, setErasing] = useState(false);
  const [erased, setErased] = useState(false);
  const revoke = useRevokeNamespaceTokens(name);
  const erase = useEraseNamespace(name);
  const revoked = revoke.data;
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
        title="Usage"
        info={
          <p>
            {displayName ? `${displayName}, the namespace` : "The namespace"}{" "}
            <span className="mono">{ns.name}</span>: its use of the month against its limits, its
            tokens and its exceptions to the default limits. Created{" "}
            <Time ms={Date.parse(ns.createdAt)} />, active <Ago ms={Date.parse(ns.lastActiveAt)} />.
          </p>
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
                onConfirm={() => void revoke.run()}
                pending={revoke.pending}
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
      {deleting && (
        <div className="notice notice-critical" role="status">
          Being erased since <Ago ms={Date.parse(deleting)} />.
        </div>
      )}

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
      <FormDialog open={erasing && (canWrite || erase.pending)} onOpenChange={setErasing}>
        <Erase
          name={ns.name}
          label={label}
          erase={erase}
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

      <NamespaceAlerts alerts={d.alerts} month={d.month} />

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
        </div>
      </div>
    </div>
  );
}
