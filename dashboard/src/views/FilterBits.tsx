// Controls of the filter bars.
import { Plus, X } from "lucide-react";
import { type FormEvent, type ReactNode, useId, useState } from "react";

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
  return (
    <fieldset className="seg">
      <legend className="sr-only">{label}</legend>
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          aria-pressed={o.value === value}
          title={o.title}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </fieldset>
  );
}

export function FilterChip({
  name,
  value,
  onRemove,
}: {
  name: string;
  value: string;
  onRemove: () => void;
}) {
  return (
    <span className="filter-chip">
      <span className="k">{name}</span>
      <span title={value}>{value.length > 40 ? `${value.slice(0, 40)}…` : value}</span>
      <button type="button" aria-label={`Remove the ${name} filter ${value}`} onClick={onRemove}>
        <X size={12} />
      </button>
    </span>
  );
}

export interface FieldSpec {
  name: string;
  placeholder: string;
  /** Checks a value; returns why it is refused. */
  check?: (value: string) => string | null;
  suggestions?: string[];
}

/** One input to add a filter: pick the field, type, press Enter. */
export function AddFilter({
  fields,
  onAdd,
}: {
  fields: FieldSpec[];
  onAdd: (field: string, value: string) => void;
}) {
  const [field, setField] = useState(fields[0]?.name ?? "");
  const [value, setValue] = useState("");
  const [refusal, setRefusal] = useState<string | null>(null);
  const listId = useId();
  const spec = fields.find((f) => f.name === field) ?? fields[0];

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const v = value.trim();
    if (!v || !spec) return;
    const why = spec.check?.(v) ?? null;
    setRefusal(why);
    if (why) return;
    onAdd(spec.name, v);
    setValue("");
  };

  return (
    <form className="field add-filter" onSubmit={submit}>
      <select
        className="input"
        aria-label="Filter on"
        value={field}
        onChange={(e) => {
          setField(e.target.value);
          setRefusal(null);
        }}
      >
        {fields.map((f) => (
          <option key={f.name} value={f.name}>
            {f.name}
          </option>
        ))}
      </select>
      <input
        className="input mono"
        aria-label={`Filter value for ${field}`}
        aria-invalid={refusal !== null}
        placeholder={spec?.placeholder}
        value={value}
        list={spec?.suggestions?.length ? listId : undefined}
        onChange={(e) => {
          setValue(e.target.value);
          setRefusal(null);
        }}
        style={{ width: 220 }}
      />
      {spec?.suggestions && spec.suggestions.length > 0 && (
        <datalist id={listId}>
          {spec.suggestions.map((s) => (
            <option key={s} value={s} />
          ))}
        </datalist>
      )}
      <button type="submit" className="btn" disabled={!value.trim()}>
        <Plus size={13} /> Add
      </button>
      {refusal && (
        <span className="tone-critical" role="alert" style={{ fontSize: 12 }}>
          {refusal}
        </span>
      )}
    </form>
  );
}
