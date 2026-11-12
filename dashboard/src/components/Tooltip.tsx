// A hover tooltip that follows the pointer, for chart marks.
import { type MouseEvent, type ReactNode, useCallback, useState } from "react";

interface Tip {
  x: number;
  y: number;
  content: ReactNode;
}

export function useTooltip() {
  const [tip, setTip] = useState<Tip | null>(null);
  const show = useCallback((e: MouseEvent, content: ReactNode) => {
    setTip({ x: e.clientX, y: e.clientY, content });
  }, []);
  const hide = useCallback(() => setTip(null), []);
  const view = tip ? <TooltipBox {...tip} /> : null;
  return { show, hide, view };
}

function TooltipBox({ x, y, content }: Tip) {
  const width = 380;
  const left = Math.min(x + 14, window.innerWidth - width - 8);
  const top = y + 16 + 140 > window.innerHeight ? y - 12 - 140 : y + 16;
  return (
    <div
      className="tooltip"
      role="tooltip"
      style={{ left: Math.max(8, left), top: Math.max(8, top) }}
    >
      {content}
    </div>
  );
}
