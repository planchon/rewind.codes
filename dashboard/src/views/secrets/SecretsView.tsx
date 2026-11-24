// Secrets (archi/05, "Secrets"): over every namespace, the organization's secrets, which runs
// attach as `org/<name>`; in a namespace's scope, that end user's own (`ns/<name>`), then the
// organization's its runs can attach, written over every namespace. Shown to whoever holds
// `secrets:read`; written and deleted by whoever holds `secrets:write`.
import { Link } from "react-router";
import { NoPermission } from "../../app/NoPermission";
import { InfoCard } from "../../components/InfoCard";
import { PageHeader } from "../../components/PageHeader";
import { useAccess } from "../../state/access";
import { useScopeNamespace } from "../../state/scope";
import { SecretList } from "./SecretList";

export function SecretsView() {
  const access = useAccess();
  const namespace = useScopeNamespace();
  if (!access.can("secrets:read"))
    return (
      <div className="page">
        <NoPermission permission="secrets:read" what="see the secrets" />
      </div>
    );
  if (namespace !== null) return <NamespaceSecrets namespace={namespace} />;
  return (
    <div className="page">
      <PageHeader
        title="Secrets"
        info={
          <p>
            The organization's secrets: a run reads one only when it attaches it at start, server by
            server (<code>{'secrets: { "zendesk": "org/zendesk" }'}</code>), and only Rewind's
            dispatcher reads its value. A key limited to labels sees and attaches only those
            carrying them. Each end user's own secrets are in the Secrets of their namespace, picked
            at the top of the page.
          </p>
        }
      />
      <SecretList namespace={null} />
    </div>
  );
}

/** A namespace's secrets, then the organization's that its runs can attach. */
function NamespaceSecrets({ namespace }: { namespace: string }) {
  const access = useAccess();
  return (
    <div className="page">
      <PageHeader
        title="Secrets"
        info={
          <p>
            This end user's own values, such as their GitHub token: only the runs of this namespace
            attach them, as <code>ns/&lt;name&gt;</code>, and nothing falls back on the
            organization's.
            {access.can("secrets:write") && access.limited
              ? " Your grant is limited to labels: it reads them, and never writes one."
              : ""}
          </p>
        }
      />
      <section className="ns-section" aria-label="Its secrets">
        <SecretList namespace={namespace} />
      </section>
      <section className="ns-section" aria-label="The organization's secrets">
        <header className="ns-section-head">
          <h2>
            The organization's
            <InfoCard label="About the organization's secrets">
              <p>
                The runs of every namespace attach them, as <code>org/&lt;name&gt;</code>. They are
                written in the <Link to="/secrets">Secrets of every namespace</Link>.
              </p>
            </InfoCard>
          </h2>
        </header>
        <SecretList namespace={null} readOnly />
      </section>
    </div>
  );
}
