// Browser storage for per-viewer conveniences: the key, the theme, the time zone. Storage can be
// missing or throw (private windows, blocked site data): every access is guarded.

export type StorageKind = "local" | "session";

function area(kind: StorageKind): Storage | null {
  try {
    return kind === "local" ? window.localStorage : window.sessionStorage;
  } catch {
    return null;
  }
}

export function read(kind: StorageKind, key: string): string | null {
  try {
    return area(kind)?.getItem(key) ?? null;
  } catch {
    return null;
  }
}

export function write(kind: StorageKind, key: string, value: string | null): void {
  try {
    const storage = area(kind);
    if (!storage) return;
    if (value === null) storage.removeItem(key);
    else storage.setItem(key, value);
  } catch {
    // Not stored: the setting lasts until the page is closed.
  }
}
