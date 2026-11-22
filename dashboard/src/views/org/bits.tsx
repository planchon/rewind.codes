// Pieces the organization views share: the role picker that offers only what the caller may
// give, the allowed labels field, inline confirmations, people and principals.
import { type ReactNode, useEffect, useId, useMemo, useState } from "react";
import { Avatar } from "../../app/Brand";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { NamespaceName } from "../../components/NamespaceName";
import {
  canGive,
  displayName,
  formatLabels,
  missingPermissions,
  OPERATOR_ROLE,
  parseLabels,
  splitPrincipal,
} from "../../model/access";
import type { ApiKey, Labels, Member, Permission, Role } from "../../model/types";
import { useAccess } from "../../state/access";
import { useMembers } from "../../state/organization";

/** A labelled field with its hint, or its error, described to assistive technologies. */
export function Field({
  label,
  hint,
  error = null,
  grow = false,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  grow?: boolean;
  children: (props: { id: string; "aria-describedby": string; "aria-invalid"?: true }) => ReactNode;
}) {
  const id = useId();
  const hintId = `${id}-hint`;
  return (
    <div className={grow ? "field col grow" : "field col"}>
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      {children({
        id,
        "aria-describedby": hintId,
        ...(error ? { "aria-invalid": true as const } : {}),
      })}
      <span id={hintId} className={error ? "field-hint tone-critical" : "field-hint"}>
        {error ?? hint}
      </span>
    </div>
  );
}

/** The roles the caller may give: every permission of the role is one it holds. */
export function givableRoles(roles: readonly Role[], held: ReadonlySet<Permission>): Role[] {
  return roles.filter((r) => canGive(r, held));
}

export function RoleSelect({
  roles,
  value,
  onChange,
  label = "Role",
  operator = false,
}: {
  roles: readonly Role[];
  value: string;
  onChange: (role: string) => void;
  label?: string;
  /** Offers `operator`, which only the operator gives. */
  operator?: boolean;
}) {
  const chosen = roles.find((r) => r.name === value);
  const hint =
    value === OPERATOR_ROLE
      ? "Every permission, and the keys of every tenant."
      : (chosen?.description ?? "");
  return (
    <Field label={label} hint={hint}>
      {(props) => (
        <select
          {...props}
          className="input"
          value={value}
          onChange={(e) => onChange(e.target.value)}
        >
          {roles.map((r) => (
            <option key={r.name} value={r.name}>
              {r.name}
              {r.builtIn ? "" : " (custom)"}
            </option>
          ))}
          {operator && <option value={OPERATOR_ROLE}>operator</option>}
        </select>
      )}
    </Field>
  );
}

/**
 * The role to give: the roles the caller may give, or, when its role cannot list the roles
 * (`roles:read`), the role's name typed.
 */
export function RoleField({
  roles,
  value,
  onChange,
  operator = false,
}: {
  /** The givable roles; `null` when they cannot be listed. */
  roles: readonly Role[] | null;
  value: string;
  onChange: (role: string) => void;
  operator?: boolean;
}) {
  if (roles)
    return <RoleSelect roles={roles} value={value} onChange={onChange} operator={operator} />;
  return (
    <Field label="Role" hint="Your role cannot list the roles: type the role's name.">
      {(props) => (
        <input
          {...props}
          className="input mono"
          value={value}
          spellCheck={false}
          onChange={(e) => onChange(e.target.value.trim())}
        />
      )}
    </Field>
  );
}

/** The role a form starts with: the least of those offered (the last built-in, `reader`). */
export function defaultRole(roles: readonly Role[] | null): string {
  if (!roles) return "reader";
  return roles.find((r) => r.name === "reader")?.name ?? roles[roles.length - 1]?.name ?? "";
}

/** Allowed labels, typed as `key:value` pairs. */
export function LabelsField({
  value,
  onChange,
  error,
}: {
  value: string;
  onChange: (text: string) => void;
  error: string | null;
}) {
  return (
    <LabelsInput
      label="Allowed labels"
      value={value}
      onChange={onChange}
      error={error}
      placeholder="agent:triage conversation:c-42"
      hint="Optional. Only runs carrying all of them are seen; such a grant cannot administer."
    />
  );
}

/** Labels typed as `key:value` pairs, with a label and a hint of the caller's. */
export function LabelsInput({
  label,
  value,
  onChange,
  error,
  placeholder,
  hint,
}: {
  label: string;
  value: string;
  onChange: (text: string) => void;
  error: string | null;
  placeholder: string;
  hint: ReactNode;
}) {
  return (
    <Field label={label} error={error} hint={hint}>
      {(props) => (
        <input
          {...props}
          className="input mono"
          placeholder={placeholder}
          spellCheck={false}
          value={value}
          onChange={(e) => onChange(e.target.value)}
        />
      )}
    </Field>
  );
}

/** The labels being typed, and what they read as. */
export function useLabels(initial?: Labels) {
  const [text, setText] = useState(() => formatLabels(initial));
  const parsed = parseLabels(text);
  return {
    text,
    setText,
    labels: "labels" in parsed ? parsed.labels : null,
    error: "error" in parsed ? parsed.error : null,
  };
}

/**
 * A button that asks once more, in a dialog, before doing something that cannot be undone: the
 * question, what happens, and a red button named like this one. The dialog closes once the change
 * is done; its error shows where the page shows it.
 */
export function ConfirmButton({
  children,
  confirm,
  description,
  onConfirm,
  disabled = false,
  pending = false,
  title,
  icon,
  small = true,
}: {
  /** The button's label, and the dialog's red button's: `Revoke`, `Delete`. */
  children: string;
  /** Before the label, as the actions of a page's header have one. */
  icon?: ReactNode;
  /** A small button, in a row; a page's header has the regular size of its other actions. */
  small?: boolean;
  /** The question, such as `Revoke the key ci?`. */
  confirm: string;
  /** What happens, and what remains. */
  description?: ReactNode;
  onConfirm: () => void;
  disabled?: boolean;
  pending?: boolean;
  title?: string;
}) {
  const [asking, setAsking] = useState(false);
  const [sent, setSent] = useState(false);
  // Closes once the change sent from the dialog is done, whatever its outcome.
  useEffect(() => {
    if (sent && !pending) {
      setSent(false);
      setAsking(false);
    }
  }, [sent, pending]);
  return (
    <>
      <button
        type="button"
        className={small ? "btn small" : "btn"}
        disabled={disabled || pending}
        title={title}
        onClick={() => setAsking(true)}
      >
        {icon}
        {children}
      </button>
      <ConfirmDialog
        open={asking}
        onOpenChange={setAsking}
        title={confirm}
        description={description}
        confirm={children}
        pendingLabel={`${children}…`}
        pending={pending}
        onConfirm={() => {
          setSent(true);
          onConfirm();
        }}
      />
    </>
  );
}

export function Person({ member, you }: { member: Member; you: boolean }) {
  const { user } = member;
  return (
    <span className="person">
      <Avatar user={user} />
      <span className="person-text">
        <span className="person-name">
          {displayName(user)}
          {you && <span className="tag">you</span>}
        </span>
        <span className="muted">{user.email}</span>
      </span>
    </span>
  );
}

/** The members, when the caller may list them: to name who invited, granted or created. */
export function useMemberNames(): Map<string, string> {
  const access = useAccess();
  const members = useMembers(access.can("members:read") && access.organization !== null);
  return useMemo(
    () => new Map((members.data ?? []).map((m) => [m.user.id, displayName(m.user)])),
    [members.data],
  );
}

/**
 * Who a principal's text (`user:<id>`, `key:<id>`, `connection:<id>`, or an end user's token,
 * `token:<namespace>`) is, as well as it is known.
 */
export function PrincipalName({
  text,
  names,
  keys = [],
}: {
  text: string | null | undefined;
  names: Map<string, string>;
  keys?: readonly ApiKey[];
}) {
  if (!text) return <span className="muted">the server</span>;
  const { kind, id } = splitPrincipal(text);
  if (kind === "user") {
    const name = names.get(id);
    return name ? <span title={id}>{name}</span> : <span className="mono">{id}</span>;
  }
  if (kind === "key") {
    const key = keys.find((k) => k.id === id);
    return (
      <span title={text}>
        key <span className="mono">{key?.name ?? id.slice(0, 8)}</span>
      </span>
    );
  }
  if (kind === "token") {
    return (
      <span title={text}>
        the token of <NamespaceName name={id} />
      </span>
    );
  }
  if (kind === "connection") {
    return (
      <span title={text}>
        connection <span className="mono">{id.slice(0, 11)}</span>
      </span>
    );
  }
  return <span className="mono">{text}</span>;
}

/** Why the caller cannot change a principal holding this role: it lacks some of its permissions. */
export function beyondCaller(
  role: string,
  roles: readonly Role[],
  held: ReadonlySet<Permission>,
): Permission[] {
  const found = roles.find((r) => r.name === role);
  if (!found) return [];
  return missingPermissions(found.permissions, held);
}

/** A description of the API's, its `code` spans shown as code. */
export function Described({ text }: { text: string }) {
  return (
    <>
      {text.split("`").map((part, i) =>
        // Odd parts were between backticks.
        // biome-ignore lint/suspicious/noArrayIndexKey: the parts of a fixed text
        i % 2 === 1 ? <code key={i}>{part}</code> : <span key={i}>{part}</span>,
      )}
    </>
  );
}

export function PermissionList({ permissions }: { permissions: readonly Permission[] }) {
  if (permissions.length === 0) return <span className="muted">no permission</span>;
  return (
    <span className="perm-chips">
      {permissions.map((p) => (
        <span key={p} className="perm-chip">
          {p}
        </span>
      ))}
    </span>
  );
}
