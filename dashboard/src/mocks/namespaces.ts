// The mocked namespaces (archi/06, "Namespaces" and "Quotas"): the end users of the tenant, their
// exceptions to the defaults, the operator's ceiling, their use day by day (CPU time and runs
// started) over this month and the last, the runs active now, their distinct scripts, their
// tokens, and the alerts recorded when a use crossed 80 % or 100 % of a limit. A few named end
// users tell a story (bob abuses, chen pays, erin is blocked); a thousand more fill the lists.
import type {
  Namespace,
  NamespaceDefaults,
  NamespaceDetail,
  QuotaAlert,
  QuotaKind,
  Quotas,
  QuotaUsage,
} from "../api/types";
import { apiDuration, limitOf, monthOf, shiftMonth } from "../model/quotas";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

export interface DayUse {
  cpuMs: number;
  runsStarted: number;
}

export interface MockNamespace {
  name: string;
  /** The name the organization gave it, shown instead of its id. */
  displayName: string | null;
  createdAt: number;
  exceptions: Quotas;
  lastActiveAt: number;
  deletingSince: number | null;
  /** Use per UTC day, `2026-11-14`. */
  days: Map<string, DayUse>;
  /** Runs active now. */
  activeRuns: number;
  /** Distinct scripts with a run kept. */
  scripts: number;
  /** Opaque tokens neither revoked nor expired. */
  tokens: number;
  tokensRevokedBefore: number | null;
}

export interface NamespaceWorld {
  defaults: Quotas;
  /** The operator's: it bounds the tenant's totals. */
  ceiling: Quotas;
  namespaces: Map<string, MockNamespace>;
  /** Every alert recorded, of the namespaces and of the tenant (`namespace: null`). */
  alerts: QuotaAlert[];
}

/** An empty world: the tenant has no end user yet. */
export function emptyNamespaceWorld(): NamespaceWorld {
  return { defaults: {}, ceiling: {}, namespaces: new Map(), alerts: [] };
}

function seeded(seed: number): () => number {
  let a = seed;
  return () => {
    a += 0x6d2b79f5;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4_294_967_296;
  };
}

function iso(ms: number): string {
  return new Date(ms).toISOString();
}

/** Midnight UTC of a month's first day. */
function monthStart(month: string): number {
  return Date.parse(`${month}-01T00:00:00Z`);
}

/** A namespace created at first use, with the defaults and no use yet. */
export function newNamespace(name: string, createdAt: number): MockNamespace {
  return {
    name,
    displayName: null,
    createdAt,
    exceptions: {},
    lastActiveAt: createdAt,
    deletingSince: null,
    days: new Map(),
    activeRuns: 0,
    scripts: 0,
    tokens: 0,
    tokensRevokedBefore: null,
  };
}

/** The limits that apply: per quota, the namespace's exception, else the default. */
export function limitsOf(world: NamespaceWorld, ns: MockNamespace): Quotas {
  const limits: Quotas = { ...world.defaults };
  for (const [key, value] of Object.entries(ns.exceptions)) {
    if (value !== null && value !== undefined) (limits as Record<string, unknown>)[key] = value;
  }
  return limits;
}

function monthUse(ns: MockNamespace, month: string): DayUse {
  let cpuMs = 0;
  let runsStarted = 0;
  for (const [day, use] of ns.days) {
    if (!day.startsWith(`${month}-`)) continue;
    cpuMs += use.cpuMs;
    runsStarted += use.runsStarted;
  }
  return { cpuMs, runsStarted };
}

export function usageOf(ns: MockNamespace, month: string): QuotaUsage {
  const use = monthUse(ns, month);
  return {
    activeRuns: ns.activeRuns,
    runsStarted: use.runsStarted,
    cpu: apiDuration(use.cpuMs),
    scripts: ns.scripts,
  };
}

/** The tenant's use of a month: the sum over its namespaces, erased ones included. */
export function tenantUsage(world: NamespaceWorld, month: string): QuotaUsage {
  let cpuMs = 0;
  const usage = { activeRuns: 0, runsStarted: 0, scripts: 0 };
  for (const ns of world.namespaces.values()) {
    const use = monthUse(ns, month);
    cpuMs += use.cpuMs;
    usage.runsStarted += use.runsStarted;
    usage.activeRuns += ns.activeRuns;
    usage.scripts += ns.scripts;
  }
  return { ...usage, cpu: apiDuration(cpuMs) };
}

export function namespaceDto(world: NamespaceWorld, ns: MockNamespace, month: string): Namespace {
  const dto: Namespace = {
    name: ns.name,
    createdAt: iso(ns.createdAt),
    lastActiveAt: iso(ns.lastActiveAt),
    exceptions: ns.exceptions,
    limits: limitsOf(world, ns),
    usage: usageOf(ns, month),
  };
  if (ns.displayName !== null) dto.displayName = ns.displayName;
  if (ns.deletingSince !== null) dto.deletingSince = iso(ns.deletingSince);
  return dto;
}

export function alertsOf(
  world: NamespaceWorld,
  month: string,
  namespace?: string | null,
): QuotaAlert[] {
  return world.alerts
    .filter((a) => a.period === month && (namespace === undefined || a.namespace === namespace))
    .sort((a, b) => b.raisedAt.localeCompare(a.raisedAt));
}

export function detailDto(
  world: NamespaceWorld,
  ns: MockNamespace,
  month: string,
): NamespaceDetail {
  const detail: NamespaceDetail = {
    month,
    namespace: namespaceDto(world, ns, month),
    days: [...ns.days]
      .filter(([day]) => day.startsWith(`${month}-`))
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([day, use]) => ({ day, cpu: apiDuration(use.cpuMs), runsStarted: use.runsStarted })),
    alerts: alertsOf(world, month, ns.name),
    tokens: ns.tokens,
  };
  if (ns.tokensRevokedBefore !== null) detail.tokensRevokedBefore = iso(ns.tokensRevokedBefore);
  return detail;
}

export function defaultsDto(world: NamespaceWorld, month: string): NamespaceDefaults {
  return {
    month,
    defaults: world.defaults,
    ceiling: world.ceiling,
    usage: tenantUsage(world, month),
  };
}

/**
 * The highest share of a limit a namespace used in the month, as `above` reads it: a limit of 0
 * (a blocked end user) is never reached.
 */
export function highest(world: NamespaceWorld, ns: MockNamespace, month: string): number {
  const limits = limitsOf(world, ns);
  const use = monthUse(ns, month);
  const pairs: [number, number | null][] = [
    [use.cpuMs, limitOf(limits, "cpuPerMonth")],
    [ns.activeRuns, limitOf(limits, "activeRuns")],
    [use.runsStarted, limitOf(limits, "runsPerMonth")],
    [ns.scripts, limitOf(limits, "scripts")],
  ];
  let best = 0;
  for (const [used, limit] of pairs) {
    if (limit !== null && limit > 0) best = Math.max(best, used / limit);
  }
  return best;
}

/**
 * Spreads a month's totals over its days, from the 1st to `lastDay`, the last days heavier: an
 * end user whose use grows.
 */
function spread(
  ns: MockNamespace,
  month: string,
  lastDay: number,
  cpuMs: number,
  runs: number,
  random: () => number,
  growth = 0.15,
) {
  const weights = Array.from({ length: lastDay }, (_, i) => 1 + i * growth + random() * 0.4);
  const sum = weights.reduce((a, b) => a + b, 0);
  let cpuLeft = cpuMs;
  let runsLeft = runs;
  weights.forEach((w, i) => {
    const last = i === weights.length - 1;
    const cpu = last ? cpuLeft : Math.round((cpuMs * w) / sum);
    const started = last ? runsLeft : Math.round((runs * w) / sum);
    cpuLeft -= cpu;
    runsLeft -= started;
    const day = `${month}-${String(i + 1).padStart(2, "0")}`;
    if (cpu > 0 || started > 0) ns.days.set(day, { cpuMs: cpu, runsStarted: started });
  });
}

/**
 * Records the alerts a month's use crossed: CPU and runs as they added up day after day,
 * scripts when the stock reached the threshold, active runs at a first refusal.
 */
function recordAlerts(
  world: NamespaceWorld,
  ns: MockNamespace,
  month: string,
  now: number,
  refusedActiveRunsOn: number | null = null,
) {
  const limits = limitsOf(world, ns);
  const days = [...ns.days]
    .filter(([day]) => day.startsWith(`${month}-`))
    .sort(([a], [b]) => a.localeCompare(b));
  const raise = (kind: QuotaKind, threshold: number, used: number, limit: number, at: number) => {
    const raisedAt = Math.min(at, now - 5 * MINUTE);
    world.alerts.push({
      namespace: ns.name,
      quota: kind,
      threshold,
      used,
      limit,
      period: month,
      raisedAt: iso(raisedAt),
      sentAt: iso(raisedAt + 3_000),
    });
  };
  const cumulative = (kind: QuotaKind, limit: number | null, pick: (u: DayUse) => number) => {
    if (limit === null || limit === 0) return;
    let total = 0;
    for (const [day, use] of days) {
      const before = total;
      total += pick(use);
      for (const threshold of [80, 100]) {
        if (before * 100 < limit * threshold && limit * threshold <= total * 100)
          raise(kind, threshold, total, limit, Date.parse(`${day}T14:20:00Z`));
      }
    }
  };
  cumulative("cpu_per_month", limitOf(limits, "cpuPerMonth"), (u) => u.cpuMs);
  cumulative("runs_per_month", limitOf(limits, "runsPerMonth"), (u) => u.runsStarted);
  const scripts = limitOf(limits, "scripts");
  const lastDay = days[days.length - 1]?.[0];
  if (scripts !== null && scripts > 0 && lastDay) {
    for (const threshold of [80, 100]) {
      if (ns.scripts * 100 >= scripts * threshold)
        raise("scripts", threshold, ns.scripts, scripts, Date.parse(`${lastDay}T09:40:00Z`) - DAY);
    }
  }
  const active = limitOf(limits, "activeRuns");
  if (refusedActiveRunsOn !== null && active !== null)
    raise("active_runs", 100, active, active, refusedActiveRunsOn);
}

interface Story {
  name: string;
  displayName?: string;
  createdDaysAgo: number;
  exceptions?: Quotas;
  /** This month's totals, up to today. */
  cpuMs: number;
  runs: number;
  activeRuns: number;
  scripts: number;
  tokens: number;
  lastActiveAgoMs: number;
  /** The last month's totals. */
  last?: { cpuMs: number; runs: number };
  /** A start refused for `activeRuns`, this many days ago. */
  refusedDaysAgo?: number;
}

/** The end users the mocked runs act for, and those whose use tells something. */
const STORIES: Story[] = [
  {
    // Abuses the product: above its CPU, at its active runs, close to its runs and scripts.
    name: "bob",
    createdDaysAgo: 43,
    cpuMs: 3 * HOUR + 12 * MINUTE,
    runs: 2_950,
    activeRuns: 10,
    scripts: 412,
    tokens: 3,
    lastActiveAgoMs: 40_000,
    last: { cpuMs: 74 * MINUTE, runs: 1_820 },
    refusedDaysAgo: 2,
  },
  {
    // Pays for more: its exceptions lift the defaults.
    name: "chen",
    displayName: "Chen Logistics",
    createdDaysAgo: 120,
    exceptions: { runsPerMonth: 20_000, cpuPerMonth: "10h", scripts: 2_000 },
    cpuMs: 2 * HOUR + 45 * MINUTE,
    runs: 4_120,
    activeRuns: 4,
    scripts: 220,
    tokens: 5,
    lastActiveAgoMs: 3 * MINUTE,
    last: { cpuMs: 5 * HOUR + 10 * MINUTE, runs: 7_300 },
  },
  {
    name: "dana",
    createdDaysAgo: 31,
    cpuMs: 1 * HOUR + 41 * MINUTE,
    runs: 1_480,
    activeRuns: 3,
    scripts: 64,
    tokens: 1,
    lastActiveAgoMs: 12 * MINUTE,
    last: { cpuMs: 52 * MINUTE, runs: 900 },
  },
  {
    name: "alice",
    createdDaysAgo: 88,
    cpuMs: 41 * MINUTE,
    runs: 312,
    activeRuns: 2,
    scripts: 57,
    tokens: 2,
    lastActiveAgoMs: 4 * MINUTE,
    last: { cpuMs: 66 * MINUTE, runs: 540 },
  },
  {
    // Blocked by the customer: no run may start.
    name: "erin",
    createdDaysAgo: 64,
    exceptions: { activeRuns: 0 },
    cpuMs: 9 * MINUTE,
    runs: 41,
    activeRuns: 0,
    scripts: 12,
    tokens: 0,
    lastActiveAgoMs: 2 * DAY,
    last: { cpuMs: 47 * MINUTE, runs: 610 },
  },
  {
    name: "local",
    createdDaysAgo: 200,
    cpuMs: 6 * MINUTE,
    runs: 58,
    activeRuns: 1,
    scripts: 9,
    tokens: 0,
    lastActiveAgoMs: 20 * MINUTE,
  },
  {
    name: "user_2fQ81",
    displayName: "Northwind Travel",
    createdDaysAgo: 18,
    cpuMs: 14 * MINUTE,
    runs: 160,
    activeRuns: 0,
    scripts: 21,
    tokens: 1,
    lastActiveAgoMs: 5 * HOUR,
  },
  {
    name: "user_7Kd02",
    displayName: "Acme Corp",
    createdDaysAgo: 9,
    cpuMs: 22 * MINUTE,
    runs: 2_480,
    activeRuns: 1,
    scripts: 33,
    tokens: 2,
    lastActiveAgoMs: 50 * MINUTE,
  },
];

const ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZabcdefghijkmnopqrstuvwxyz";

export interface NamespaceOptions {
  now: number;
  /** The namespaces of the mocked runs: each exists, with some use. */
  names: Iterable<string>;
  /** How many more end users, with less use. */
  others?: number;
}

/** The tenant's namespaces, their defaults, the ceiling, their use and their alerts. */
export function createNamespaceWorld({
  now,
  names,
  others = 1_200,
}: NamespaceOptions): NamespaceWorld {
  const world: NamespaceWorld = {
    defaults: {
      activeRuns: 10,
      runsPerMonth: 3_000,
      cpuPerMonth: "2h",
      scripts: 500,
      maxTtl: "7d",
      maxScriptLines: 300,
    },
    ceiling: {
      activeRuns: 5_000,
      runsPerMonth: 1_000_000,
      cpuPerMonth: "1000h",
      scripts: 500_000,
      maxTtl: "30d",
      maxScriptLines: 2_000,
    },
    namespaces: new Map(),
    alerts: [],
  };
  const random = seeded(11);
  const month = monthOf(now);
  const last = shiftMonth(month, -1);
  const today = new Date(now).getUTCDate();
  const lastDays = new Date(monthStart(month) - DAY).getUTCDate();
  const add = (name: string, createdAt: number, lastActiveAt: number): MockNamespace => {
    const ns = newNamespace(name, createdAt);
    ns.lastActiveAt = lastActiveAt;
    world.namespaces.set(name, ns);
    return ns;
  };

  for (const s of STORIES) {
    const ns = add(s.name, now - s.createdDaysAgo * DAY, now - s.lastActiveAgoMs);
    if (s.exceptions) ns.exceptions = s.exceptions;
    if (s.displayName) ns.displayName = s.displayName;
    ns.activeRuns = s.activeRuns;
    ns.scripts = s.scripts;
    ns.tokens = s.tokens;
    if (s.last && s.createdDaysAgo > today)
      spread(ns, last, lastDays, s.last.cpuMs, s.last.runs, random, 0.05);
    spread(ns, month, today, s.cpuMs, s.runs, random);
    if (s.last) recordAlerts(world, ns, last, monthStart(month));
    const refused = s.refusedDaysAgo === undefined ? null : now - s.refusedDaysAgo * DAY;
    recordAlerts(world, ns, month, now, refused);
  }

  // The end users of the mocked runs that have no story.
  for (const name of names) {
    if (world.namespaces.has(name)) continue;
    const ns = add(name, now - 30 * DAY, now - HOUR);
    spread(ns, month, today, 10 * MINUTE, 90, random);
  }

  // Many more, most of them quiet.
  for (let i = 0; i < others; i += 1) {
    let name = "user_";
    for (let k = 0; k < 5; k += 1) name += ALPHABET[Math.floor(random() * ALPHABET.length)] ?? "x";
    if (world.namespaces.has(name)) continue;
    const created = now - Math.floor(random() * 200 + 1) * DAY;
    const quiet = random() < 0.3;
    const ns = add(name, created, quiet ? created + DAY : now - Math.floor(random() * 6 * DAY));
    if (quiet) continue;
    const size = random() ** 3;
    ns.activeRuns = random() < 0.15 ? 1 + Math.floor(random() * 3) : 0;
    ns.scripts = 1 + Math.floor(size * 220);
    ns.tokens = Math.floor(random() * 3);
    spread(
      ns,
      month,
      today,
      Math.round(size * 70 * MINUTE),
      Math.round(size * 1_400) + 1,
      random,
      0.05,
    );
  }
  return world;
}
