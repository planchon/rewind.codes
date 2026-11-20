// Signing keys: the organization's Ed25519 keys, with which Rewind signs the calls to the servers
// and APIs whose `auth` is `rewind` (archi/04, "Signed calls"); the issuer and the address of the
// public keys, to copy into the server that checks them (`verifyRewindCall`). Rotating the key,
// with `organization:write`, is confirmed in a dialog.
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RotateCw } from "lucide-react";
import { SIGNING_KEYS, useSigningKeys } from "../../api/secrets";
import { useAccess } from "../../app/access";
import { useApi } from "../../app/session";
import { Ago, CopyButton, Loading, Time } from "../../components/bits";
import { ErrorNote } from "../../components/ErrorNote";
import { ConfirmButton } from "./bits";
import { SectionGate } from "./OrganizationView";

const PUBLISHED_MS = 24 * 3_600_000;

export function SigningKeysView() {
  return (
    <SectionGate path="signing-keys">
      <SigningKeys />
    </SectionGate>
  );
}

function SigningKeys() {
  const api = useApi();
  const access = useAccess();
  const queryClient = useQueryClient();
  const keys = useSigningKeys();
  const rotate = useMutation({
    mutationFn: () => api.rotateSigningKey(),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: SIGNING_KEYS }),
  });
  return (
    <section className="org-section" aria-label="Signing keys">
      <div className="section-bar">
        <p className="section-intro dim">
          Rewind signs each call to a server or an API whose <code>auth</code> is{" "}
          <code>{'{ "type": "rewind" }'}</code> with this organization's Ed25519 key: a JWT naming
          the end user (<code>sub</code>), the run, its labels and the request. The server checks it
          against the public keys below, with <code>verifyRewindCall</code> of the SDK.
        </p>
        {access.can("organization:write") && (
          <ConfirmButton
            confirm="Rotate the signing key?"
            description="A new key signs every call from now on. The current one stops signing at once and stays published for 24 hours, so the calls it signed are still accepted; a server fetches the new key when it meets its id."
            onConfirm={() => rotate.mutate()}
            pending={rotate.isPending}
            icon={<RotateCw size={12} aria-hidden="true" />}
            small={false}
          >
            Rotate the key
          </ConfirmButton>
        )}
      </div>
      {rotate.error ? <ErrorNote error={rotate.error} title="The key was not rotated" /> : null}
      {keys.isPending ? (
        <Loading />
      ) : keys.error ? (
        <ErrorNote error={keys.error} title="The signing keys could not be read" />
      ) : (
        <>
          <dl className="kv signing-kv">
            <div>
              <dt>Issuer</dt>
              <dd>
                <span className="copyable">
                  <span className="mono" data-testid="issuer">
                    {keys.data.issuer}
                  </span>
                  <CopyButton text={keys.data.issuer} label="Copy the issuer" />
                </span>
              </dd>
            </div>
            <div>
              <dt>Public keys</dt>
              <dd>
                <span className="copyable">
                  <a
                    className="mono"
                    href={keys.data.jwksUrl}
                    target="_blank"
                    rel="noreferrer"
                    data-testid="jwks-url"
                  >
                    {keys.data.jwksUrl}
                  </a>
                  <CopyButton
                    text={keys.data.jwksUrl}
                    label="Copy the address of the public keys"
                  />
                </span>
              </dd>
            </div>
          </dl>
          {keys.data.keys.length === 0 ? (
            <p className="muted">
              No key yet: the first toolset that names a server with <code>auth: rewind</code> makes
              it.
            </p>
          ) : (
            <div className="table-wrap">
              <table className="grid">
                <thead>
                  <tr>
                    <th>Key id</th>
                    <th>Made</th>
                    <th>State</th>
                  </tr>
                </thead>
                <tbody>
                  {keys.data.keys.map((k) => (
                    <tr key={k.kid} className={k.retiredAt ? "dimmed" : undefined}>
                      <td>
                        <span className="copyable">
                          <span className="mono">{k.kid}</span>
                          <CopyButton text={k.kid} label={`Copy the key id ${k.kid}`} />
                        </span>
                      </td>
                      <td className="dim">
                        <Ago ms={Date.parse(k.createdAt)} />
                      </td>
                      <td>
                        {k.retiredAt ? (
                          <span className="tag">
                            retired <Ago ms={Date.parse(k.retiredAt)} />, published until{" "}
                            <Time ms={Date.parse(k.retiredAt) + PUBLISHED_MS} />
                          </span>
                        ) : (
                          <span className="tag key-active">signing</span>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}
    </section>
  );
}
