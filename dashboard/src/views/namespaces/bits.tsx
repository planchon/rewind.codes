// Pieces the namespace views share: a quota's use against its limit as a meter, an alert as a
// chip, a namespace's alerts of the month, the form of six limits (in a dialog), and the gate of
// `namespaces:read`.
import { type ReactNode, useState } from "react";
import { NoPermission } from "../../app/NoPermission";
import { Time } from "../../components/bits";
import { Meter } from "../../components/Charts";
import { DialogForm } from "../../components/FormDialog";
import {
  formatQuota,
  formatShare,
  type Level,
  levelOf,
  type MeteredSpec,
  monthLabel,
  parseQuotas,
  QUOTA_FIELDS,
  QUOTA_KEYS,
  QUOTA_NAMES,
  type QuotaTexts,
  quotaTexts,
  shareOf,
} from "../../model/quotas";
import type { QuotaAlert, Quotas } from "../../model/types";
import { useAccess } from "../../state/access";
import { Field } from "../org/bits";

const TONE: Record<Level, "info" | "warning" | "critical" | null> = {
  none: null,
  ok: "info",
  warning: "warning",
  critical: "critical",
};

/**
 * A use against its limit: a bar, orange from 80 %, red at 100 %, and the figures. CPU time is
 * never refused: above its quota the bar stays full and the share says by how much.
 */
export function QuotaMeter({
  spec,
  used,
  limit,
}: {
  spec: MeteredSpec;
  used: number;
  limit: number | null;
}) {
  const share = shareOf(used, limit);
  const level = levelOf(share);
  const tone = TONE[level];
  const blocked = limit === 0;
  const over = share !== null && share > 1 && Number.isFinite(share);
  const of = limit === null ? "no limit" : formatQuota(spec.key, limit);
  const label = [
    `${spec.title}: ${formatQuota(spec.key, used)}`,
    limit === null ? "no limit" : `of ${of}`,
    blocked ? "blocked" : share !== null ? formatShare(share) : null,
  ]
    .filter(Boolean)
    .join(", ");
  return (
    <span className={`quota-meter level-${level}`} role="img" aria-label={label} title={label}>
      {tone && limit !== null ? (
        <Meter value={blocked ? 1 : Math.min(used, limit)} max={blocked ? 1 : limit} tone={tone} />
      ) : (
        <span className="quota-track-none" />
      )}
      <span className="quota-figures">
        <span className="num">{formatQuota(spec.key, used)}</span>
        <span className="muted num"> / {of}</span>
        {over && <span className="quota-over num">{formatShare(share)}</span>}
        {blocked && <span className="tag critical">blocked</span>}
      </span>
    </span>
  );
}

/** An alert of the month: which quota, which threshold. */
export function AlertChip({ alert }: { alert: QuotaAlert }) {
  const tone = alert.threshold >= 100 ? "critical" : "warning";
  return (
    <span className={`tag ${tone}`}>
      {QUOTA_NAMES[alert.quota]} {alert.threshold}%
    </span>
  );
}

/**
 * A namespace's alerts of the month, what needs attention first on its Overview and its Usage;
 * nothing when it has none.
 */
export function NamespaceAlerts({
  alerts,
  month,
}: {
  alerts: readonly QuotaAlert[];
  /** `2026-11`. */
  month: string;
}) {
  if (alerts.length === 0) return null;
  return (
    <section className="panel" aria-label="Alerts">
      <div className="panel-head">
        <h2>Alerts in {monthLabel(month)}</h2>
      </div>
      <ul className="alert-list">
        {alerts.map((a) => (
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
    </section>
  );
}

/** The most telling alert of a namespace: the highest threshold, the latest. */
export function worstAlert(alerts: readonly QuotaAlert[]): QuotaAlert | null {
  let worst: QuotaAlert | null = null;
  for (const a of alerts) {
    if (
      !worst ||
      a.threshold > worst.threshold ||
      (a.threshold === worst.threshold && a.raisedAt > worst.raisedAt)
    )
      worst = a;
  }
  return worst;
}

/**
 * The six limits as a form, in the dialog that opened it: an empty field sets none (`blank`). The
 * figures are checked before they are sent, each under its field.
 */
export function QuotasForm({
  label,
  intro,
  initial,
  blank,
  submit,
  pending,
  error,
  onSubmit,
}: {
  label: string;
  intro: ReactNode;
  initial: Quotas | undefined;
  /** What an empty field means, per quota: `default: 10`, `no limit`. */
  blank: (key: keyof Quotas) => string;
  submit: string;
  pending: boolean;
  error: unknown;
  onSubmit: (quotas: Quotas) => void;
}) {
  const [texts, setTexts] = useState<QuotaTexts>(() => quotaTexts(initial));
  const [errors, setErrors] = useState<Partial<QuotaTexts>>({});
  const send = () => {
    const parsed = parseQuotas(texts);
    if (parsed.errors) {
      setErrors(parsed.errors);
      return;
    }
    setErrors({});
    onSubmit(parsed.quotas);
  };
  return (
    <DialogForm
      title={label}
      description={intro}
      submit={submit}
      pending={pending}
      error={error}
      onSubmit={send}
    >
      <div className="quota-form">
        {QUOTA_FIELDS.map((f) => (
          <Field key={f.key} label={f.label} hint={f.hint} error={errors[f.key] ?? null}>
            {(props) => (
              <input
                {...props}
                className="input mono"
                inputMode={f.unit === "count" ? "numeric" : "text"}
                spellCheck={false}
                placeholder={blank(f.key)}
                value={texts[f.key]}
                onChange={(e) => setTexts((t) => ({ ...t, [f.key]: e.target.value }))}
              />
            )}
          </Field>
        ))}
      </div>
    </DialogForm>
  );
}

/** The namespace views need `namespaces:read`. */
export function NamespacesGate({ children }: { children: ReactNode }) {
  const access = useAccess();
  if (access.can("namespaces:read")) return <>{children}</>;
  return (
    <div className="page">
      <NoPermission permission="namespaces:read" what="see the namespaces" />
    </div>
  );
}
