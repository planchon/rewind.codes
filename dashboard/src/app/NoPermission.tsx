// What the caller cannot see, said plainly, with where they can go instead.
import { Lock } from "lucide-react";
import type { ReactNode } from "react";
import { useAccess } from "./access";
import { Link } from "./namespace";

export function NoPermission({
  permission,
  what,
  children,
}: {
  permission: string;
  /** What the caller cannot do: `read runs`. */
  what: string;
  children?: ReactNode;
}) {
  const access = useAccess();
  const where = access.organization?.name;
  return (
    <div className="no-permission" role="alert">
      <h1>
        <Lock size={15} aria-hidden="true" /> You cannot {what}
        {where ? ` in ${where}` : ""}
      </h1>
      <p>
        {access.role ? (
          <>
            Your role, <span className="tag">{access.role}</span>, does not give the permission{" "}
            <code>{permission}</code>
            {access.limited && access.permissions.size < access.identity.permissions.length
              ? ", or your grant is limited to labels, which cannot administer"
              : ""}
            .
          </>
        ) : (
          <>You have no role here yet, so no permission.</>
        )}{" "}
        {access.user
          ? "Ask an owner of the organization for a role that gives it."
          : "Use a key whose role gives it."}
      </p>
      {children}
      {access.sections.length > 0 && (
        <p>
          You can open{" "}
          {access.sections.map((s, i) => (
            <span key={s.path}>
              {i > 0 ? ", " : ""}
              <Link to={`/organization/${s.path}`}>{s.title}</Link>
            </span>
          ))}
          .
        </p>
      )}
    </div>
  );
}
