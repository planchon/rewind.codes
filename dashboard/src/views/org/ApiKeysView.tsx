// API keys: the tenant's keys with their grant, who created them and when; whoever holds
// `api_keys:write` creates one (its secret is shown once) and revokes them.
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Check, Copy, KeyRound, Plus } from "lucide-react";
import { useEffect, useState } from "react";
import { API_KEYS, useApiKeys, useRoles } from "../../api/organization";
import type { ApiKey } from "../../api/types";
import { useAccess } from "../../app/access";
import { useApi } from "../../app/session";
import { Ago, Empty, LabelList, Loading } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import {
  ConfirmButton,
  defaultRole,
  Field,
  givableRoles,
  LabelsField,
  PrincipalName,
  RoleField,
  useLabels,
  useMemberNames,
} from "./bits";
import { SectionGate } from "./OrganizationView";

export function ApiKeysView() {
  return (
    <SectionGate path="api-keys">
      <ApiKeys />
    </SectionGate>
  );
}

/** The secret of a key just created: the only time anyone sees it. */
function Secret({ apiKey, onDone }: { apiKey: ApiKey; onDone: () => void }) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 1_500);
    return () => clearTimeout(t);
  }, [copied]);
  const secret = apiKey.key ?? "";
  return (
    <section className="panel secret-panel" aria-label="The new key">
      <div className="panel-head">
        <KeyRound size={13} aria-hidden="true" />
        <h2>The key {apiKey.name} was created</h2>
      </div>
      <div className="panel-body stack">
        <p>
          Copy it now and keep it somewhere safe: it is shown only this once. Send it as{" "}
          <code>Authorization: Bearer …</code>.
        </p>
        <div className="secret">
          <code className="secret-value" data-testid="secret">
            {secret}
          </code>
          <button
            type="button"
            className="btn"
            onClick={() =>
              void navigator.clipboard?.writeText(secret).then(
                () => setCopied(true),
                () => undefined,
              )
            }
          >
            {copied ? <Check size={12} /> : <Copy size={12} />} {copied ? "Copied" : "Copy"}
          </button>
        </div>
        <div>
          <button type="button" className="btn primary" onClick={onDone}>
            I have copied it
          </button>
        </div>
      </div>
    </section>
  );
}

function CreateKey({ onCreated }: { onCreated: (key: ApiKey) => void }) {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const roles = useRoles(access.can("roles:read"));
  const givable = roles.data ? givableRoles(roles.data, access.permissions) : null;
  const [name, setName] = useState("");
  const [role, setRole] = useState<string | null>(null);
  const [tenant, setTenant] = useState("");
  const labels = useLabels();
  const chosen = role ?? defaultRole(givable);
  const create = useMutation({
    mutationFn: () =>
      api.createApiKey({
        name: name.trim(),
        role: chosen,
        allowedLabels: labels.labels ?? {},
        ...(access.operator && tenant.trim() ? { tenant: tenant.trim() } : {}),
      }),
    onSuccess: async (key) => {
      await queryClient.invalidateQueries({ queryKey: API_KEYS });
      onCreated(key);
    },
  });
  const submit = () => {
    if (name.trim() && chosen && labels.labels) create.mutate();
  };
  if (roles.isPending && access.can("roles:read")) return <Loading />;
  return (
    <DialogForm
      title="New API key"
      description="For a harness, its SDK worker, the CLI or a dashboard. Its secret is shown once."
      submit="Create the key"
      pending={create.isPending}
      pendingLabel="Creating…"
      disabled={!name.trim() || !chosen || !labels.labels}
      error={create.error}
      onSubmit={submit}
    >
      <div className="form-row">
        <Field label="Name" hint="What the key is for." grow>
          {(props) => (
            <input
              {...props}
              className="input"
              placeholder="triage-harness"
              value={name}
              onChange={(e) => setName(e.target.value)}
              required
            />
          )}
        </Field>
        <RoleField roles={givable} value={chosen} onChange={setRole} operator={access.operator} />
      </div>
      <div className="form-row">
        <LabelsField value={labels.text} onChange={labels.setText} error={labels.error} />
        {access.operator && (
          <Field label="Tenant" hint="Another tenant's; it is created with its first key.">
            {(props) => (
              <input
                {...props}
                className="input mono"
                placeholder={access.identity.tenant ?? ""}
                value={tenant}
                spellCheck={false}
                onChange={(e) => setTenant(e.target.value)}
              />
            )}
          </Field>
        )}
      </div>
    </DialogForm>
  );
}

function ApiKeys() {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const keys = useApiKeys();
  const names = useMemberNames();
  const write = access.can("api_keys:write");
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<ApiKey | null>(null);
  const [failed, setFailed] = useState<{ id: string; error: unknown } | null>(null);
  const revoke = useMutation({
    mutationFn: (id: string) => api.revokeApiKey(id),
    onMutate: () => setFailed(null),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: API_KEYS }),
    onError: (error, id) => setFailed({ id, error }),
  });

  return (
    <section className="org-section" aria-label="API keys">
      <div className="section-bar">
        <p className="section-intro dim">
          Keys call the API <code>/v1</code> with the role of their grant. A key limited to labels
          sees only the runs carrying them.
        </p>
        {write && !created && (
          <button type="button" className="btn" onClick={() => setCreating(true)}>
            <Plus size={12} /> New key
          </button>
        )}
      </div>
      {created && <Secret apiKey={created} onDone={() => setCreated(null)} />}
      <FormDialog open={creating} onOpenChange={setCreating}>
        <CreateKey
          onCreated={(key) => {
            setCreating(false);
            setCreated(key);
          }}
        />
      </FormDialog>
      {keys.isPending ? (
        <Loading />
      ) : keys.error ? (
        <ErrorNote error={keys.error} />
      ) : keys.data.length === 0 ? (
        <Empty>No key yet.</Empty>
      ) : (
        <div className="table-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Name</th>
                <th>Role</th>
                <th>Allowed labels</th>
                <th>Created by</th>
                <th>Created</th>
                <th>State</th>
                {write && <th className="right">Actions</th>}
              </tr>
            </thead>
            <tbody>
              {keys.data.map((k) => (
                <tr key={k.id} className={k.revokedAt ? "dimmed" : undefined}>
                  <td>
                    <span className="key-name">{k.name}</span>{" "}
                    <span className="mono muted" title={k.id}>
                      {k.id.slice(0, 8)}
                    </span>
                    {failed?.id === k.id && <ErrorNote error={failed.error} />}
                  </td>
                  <td>
                    <span className="tag">{k.role}</span>
                  </td>
                  <td className="wrap">
                    <LabelList labels={k.allowedLabels} />
                  </td>
                  <td className="dim">
                    <PrincipalName text={k.createdBy} names={names} keys={keys.data} />
                  </td>
                  <td className="dim">
                    <Ago ms={Date.parse(k.createdAt)} />
                  </td>
                  <td>
                    {k.revokedAt ? (
                      <span className="tag">
                        revoked <Ago ms={Date.parse(k.revokedAt)} />
                      </span>
                    ) : (
                      <span className="tag key-active">active</span>
                    )}
                  </td>
                  {write && (
                    <td className="right">
                      {!k.revokedAt && (
                        <ConfirmButton
                          confirm={`Revoke ${k.name}?`}
                          description="Every client sending this key is refused from its next request. A revoked key cannot be restored: create another."
                          pending={revoke.isPending}
                          onConfirm={() => revoke.mutate(k.id)}
                        >
                          Revoke
                        </ConfirmButton>
                      )}
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
