// A namespace as every view shows it: its display name when the organization gave it one, its id
// otherwise. Hovering or focusing a display name shows the id it stands for; the id stays what
// the links, the query (`namespace:`) and the API use.
import { Tooltip } from "@base-ui/react/tooltip";
import { useNamespaceNames } from "../api/namespaces";

/** The display name of a namespace, `undefined` when it has none (or the caller cannot read it). */
export function useNamespaceLabel(): (name: string) => string | undefined {
  const names = useNamespaceNames();
  return (name: string) => names.get(name);
}

export function NamespaceName({
  name,
  displayName,
  className,
}: {
  /** The id. */
  name: string;
  /** Known already (a namespace read whole): shown as is; looked up otherwise. */
  displayName?: string | null;
  className?: string;
}) {
  const names = useNamespaceNames();
  const shown = displayName === undefined ? names.get(name) : (displayName ?? undefined);
  if (!shown) return <span className={`mono${className ? ` ${className}` : ""}`}>{name}</span>;
  return (
    <Tooltip.Root>
      <Tooltip.Trigger
        delay={250}
        render={<span className={`ns-display${className ? ` ${className}` : ""}`} />}
      >
        {shown}
      </Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Positioner
          className="ns-tip-positioner"
          side="top"
          sideOffset={6}
          collisionPadding={12}
        >
          <Tooltip.Popup className="ns-tip">
            <span className="ns-tip-label">Namespace</span> <span className="mono">{name}</span>
          </Tooltip.Popup>
        </Tooltip.Positioner>
      </Tooltip.Portal>
    </Tooltip.Root>
  );
}
