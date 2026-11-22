// Secrets: the organization's secrets, which runs attach as `org/<name>` (archi/05, "Secrets").
// Each end user's own secrets are on their namespace's page. Shown to whoever holds
// `secrets:read`; written and deleted by whoever holds `secrets:write`.
import { Link } from "react-router";
import { NoPermission } from "../../app/NoPermission";
import { PageHeader } from "../../components/PageHeader";
import { useAccess } from "../../state/access";
import { SecretList } from "./SecretList";

export function SecretsView() {
  const access = useAccess();
  if (!access.can("secrets:read"))
    return (
      <div className="page">
        <NoPermission permission="secrets:read" what="see the secrets" />
      </div>
    );
  return (
    <div className="page">
      <PageHeader title="Secrets" />
      <SecretList
        namespace={null}
        intro={
          <>
            The organization's secrets: a run reads one only when it attaches it at start, server by
            server (<code>{'secrets: { "zendesk": "org/zendesk" }'}</code>), and only Rewind's
            dispatcher reads its value. A key limited to labels sees and attaches only those
            carrying them. Each end user's own secrets are on their{" "}
            {access.can("namespaces:read") ? (
              <Link to="/namespaces">namespace's page</Link>
            ) : (
              "namespace's page"
            )}
            .
          </>
        }
      />
    </div>
  );
}
