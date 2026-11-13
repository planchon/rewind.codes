// Viewer settings: the theme (system, light, dark) and the time zone times are shown in.
import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
} from "react";
import type { TimeZoneMode } from "../model/format";
import { read, write } from "./storage";

export type ThemeChoice = "system" | "light" | "dark";

export interface Settings {
  theme: ThemeChoice;
  setTheme(theme: ThemeChoice): void;
  zone: TimeZoneMode;
  setZone(zone: TimeZoneMode): void;
}

const SettingsContext = createContext<Settings | null>(null);

function initialTheme(): ThemeChoice {
  const stored = read("local", "rewind.theme");
  return stored === "light" || stored === "dark" ? stored : "system";
}

function initialZone(): TimeZoneMode {
  return read("local", "rewind.zone") === "local" ? "local" : "utc";
}

export function SettingsProvider({ children }: { children: ReactNode }) {
  const [theme, setThemeState] = useState<ThemeChoice>(initialTheme);
  const [zone, setZoneState] = useState<TimeZoneMode>(initialZone);

  useEffect(() => {
    const root = document.documentElement;
    if (theme === "system") delete root.dataset.theme;
    else root.dataset.theme = theme;
  }, [theme]);

  const setTheme = useCallback((next: ThemeChoice) => {
    write("local", "rewind.theme", next === "system" ? null : next);
    setThemeState(next);
  }, []);

  const setZone = useCallback((next: TimeZoneMode) => {
    write("local", "rewind.zone", next === "utc" ? null : next);
    setZoneState(next);
  }, []);

  const value = useMemo(
    () => ({ theme, setTheme, zone, setZone }),
    [theme, setTheme, zone, setZone],
  );
  return <SettingsContext.Provider value={value}>{children}</SettingsContext.Provider>;
}

export function useSettings(): Settings {
  const settings = useContext(SettingsContext);
  if (!settings) throw new Error("useSettings is used outside SettingsProvider");
  return settings;
}
