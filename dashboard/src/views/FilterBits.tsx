// A segmented control: a window, an order. As on Vercel's dashboard, the choice's background
// slides from the previous choice to the new one, and a lighter one follows the pointer from
// option to option, fading in where the pointer enters and out where it leaves.
import { type ReactNode, useLayoutEffect, useRef, useState } from "react";

interface Span {
  left: number;
  width: number;
}

const same = (a: Span | null, b: Span | null) =>
  a === b || (a !== null && b !== null && a.left === b.left && a.width === b.width);

const spanOf = (el: HTMLElement): Span => ({ left: el.offsetLeft, width: el.offsetWidth });

export function Seg<T extends string>({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: T;
  options: { value: T; label: ReactNode; title?: string }[];
  onChange: (value: T) => void;
}) {
  const box = useRef<HTMLFieldSetElement>(null);
  const [thumb, setThumb] = useState<Span | null>(null);
  // Where the pointer is; `entered` when it just came in, so the highlight appears in place rather
  // than sliding from where it last left. Hidden, it stays where it was while it fades out.
  const [hover, setHover] = useState<{ span: Span; shown: boolean; entered: boolean } | null>(null);

  // The thumb under the choice, measured again when the choice or a width changes (a font that
  // loads, a label that grows).
  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    const measure = () => {
      const picked = [...el.querySelectorAll<HTMLElement>("button")].find(
        (b) => b.dataset.value === value,
      );
      const next = picked ? spanOf(picked) : null;
      setThumb((current) => (same(current, next) ? current : next));
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [value]);

  return (
    <fieldset
      className="seg sliding"
      ref={box}
      onPointerLeave={() => setHover((h) => (h ? { ...h, shown: false } : h))}
    >
      <legend className="sr-only">{label}</legend>
      <span
        className="seg-hover"
        aria-hidden="true"
        data-shown={hover?.shown ? "" : undefined}
        data-entered={hover?.entered ? "" : undefined}
        style={
          hover
            ? { transform: `translateX(${hover.span.left}px)`, width: hover.span.width }
            : undefined
        }
      />
      {thumb && (
        <span
          className="seg-thumb"
          aria-hidden="true"
          style={{ transform: `translateX(${thumb.left}px)`, width: thumb.width }}
        />
      )}
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          data-value={o.value}
          aria-pressed={o.value === value}
          title={o.title}
          onPointerEnter={(e) => {
            const span = spanOf(e.currentTarget);
            setHover((h) => ({ span, shown: true, entered: !h?.shown }));
          }}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </fieldset>
  );
}
