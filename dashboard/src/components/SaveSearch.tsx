// Saves the query of a view under a name, in this browser; the navigation lists the saved ones.
import { Star } from "lucide-react";
import { useState } from "react";
import { type SearchView, useSavedSearches } from "../app/searches";

export function SaveSearch({ q, view }: { q: string; view: SearchView }) {
  const { searches, save } = useSavedSearches();
  const [naming, setNaming] = useState(false);
  const [name, setName] = useState("");
  const saved = searches.find((s) => s.q === q.trim() && s.view === view);

  const submit = (e: { preventDefault(): void; stopPropagation(): void }) => {
    e.preventDefault();
    e.stopPropagation();
    if (!name.trim()) return;
    save({ name, q: q.trim(), view });
    setNaming(false);
  };

  if (saved)
    return (
      <span className="saved-as muted" title="Saved in this browser; the navigation lists it">
        <Star size={12} aria-hidden="true" className="tone-warning" /> {saved.name}
      </span>
    );
  if (!naming)
    return (
      <button
        type="button"
        className="btn ghost"
        onClick={() => {
          setName(q.trim().slice(0, 40) || "Every run");
          setNaming(true);
        }}
      >
        <Star size={12} aria-hidden="true" /> Save
      </button>
    );
  return (
    <span className="save-form">
      <input
        className="input"
        aria-label="Name of the saved search"
        // biome-ignore lint/a11y/noAutofocus: the operator just asked to name it
        autoFocus
        value={name}
        onChange={(e) => setName(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") submit(e);
          if (e.key === "Escape") setNaming(false);
        }}
        style={{ width: 180 }}
      />
      <button type="button" className="btn" onClick={submit} disabled={!name.trim()}>
        Save
      </button>
      <button type="button" className="btn ghost" onClick={() => setNaming(false)}>
        Cancel
      </button>
    </span>
  );
}
