// A namespace's mark: a 5 × 5 mirrored pattern drawn from its name (minidenticons), never from its
// display name, so renaming a namespace keeps it; on a tint of its own color, in both themes. The
// organization's mark is a round gradient; a namespace's is this square pattern.
import { minidenticon } from "minidenticons";
import { type CSSProperties, useMemo } from "react";

export function NamespaceMark({ name, size = 16 }: { name: string; size?: number }) {
  const { src, color } = useMemo(() => {
    const svg = minidenticon(name, 70, 48);
    return {
      src: `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`,
      color: /fill="([^"]+)"/.exec(svg)?.[1] ?? "currentColor",
    };
  }, [name]);
  return (
    <span
      className="ns-mark"
      aria-hidden="true"
      style={{ width: size, height: size, "--ns-color": color } as CSSProperties}
    >
      <img src={src} alt="" width={size} height={size} />
    </span>
  );
}
