// What the pages of Workflows share: how a test came into a suite (its tag and its glyph), the
// version's segment of the breadcrumb, and a workflow's scope.
import { Circle, CircleArrowRight, CircleDot, CircleMinus, CirclePlus } from "lucide-react";
import { NamespaceName } from "../../components/NamespaceName";
import type { WorkflowVersion } from "../../model/types";
import type { SuiteKind, TestPlace } from "../../model/workflows";
import { Switcher, type SwitchGroup } from "./Crumbs";

export const KIND_TAG: Record<
  SuiteKind | "retired",
  { label: string; className: string; title: string }
> = {
  inherited: { label: "inherited", className: "tag", title: "Kept from its base as it was" },
  added: { label: "added", className: "tag info", title: "New in this version" },
  replaced: {
    label: "replaced",
    className: "tag warning",
    title: "Replaces the base's test of the same name",
  },
  kept: {
    label: "in its suite",
    className: "tag",
    title: "Its base is not among this workflow's versions: inherited or added",
  },
  retired: {
    label: "retired",
    className: "tag critical",
    title: "A test of the base this version dropped",
  },
};

/** How a test came into the suite, as a glyph: kept, added, recorded, replaced, retired. */
export function OriginGlyph({ place }: { place: TestPlace }) {
  const size = 14;
  if (place.kind === "retired") return <CircleMinus size={size} className="origin retired" />;
  if (place.kind === "replaced")
    return <CircleArrowRight size={size} className="origin replaced" />;
  if (place.recordedFrom) return <CircleDot size={size} className="origin added" />;
  if (place.kind === "added") return <CirclePlus size={size} className="origin added" />;
  return <Circle size={size} className="origin" />;
}

export const ORIGIN_TITLE: Record<TestPlace["kind"], string> = {
  inherited: "Inherited from its base",
  added: "Added by this version",
  replaced: "Replaced by this version",
  kept: "In its suite",
  retired: "Retired by this version",
};

/** How many tests a version's suite holds: `4 tests`. */
export function testsCount(version: WorkflowVersion): string {
  const n = version.tests?.length ?? 0;
  return `${n} test${n === 1 ? "" : "s"}`;
}

/** The versions to switch to, newest first, each leading where `to` says. */
export function versionGroups(
  versions: readonly WorkflowVersion[],
  current: WorkflowVersion,
  to: (version: WorkflowVersion, latest: boolean) => string,
): SwitchGroup[] {
  return [
    {
      label: "Versions, newest first",
      items: versions.map((v, i) => ({
        key: String(v.version),
        label: <span className="num">v{v.version}</span>,
        end: `${i === 0 ? "latest · " : ""}${testsCount(v)}`,
        to: to(v, i === 0),
        current: v.version === current.version,
      })),
    },
  ];
}

/** The version's segment of the breadcrumb: its number, and whether it is the latest. */
export function VersionSwitcher({
  versions,
  current,
  to,
}: {
  versions: readonly WorkflowVersion[];
  current: WorkflowVersion;
  to: (version: WorkflowVersion, latest: boolean) => string;
}) {
  const latest = versions[0]?.version === current.version;
  return (
    <Switcher
      title="Version"
      label={
        <>
          <span className="num">v{current.version}</span>
          {latest && <span className="tag info">latest</span>}
        </>
      }
      groups={versionGroups(versions, current, to)}
    />
  );
}

/** A workflow's scope: its namespace, or the organization. */
export function Scope({ namespace }: { namespace?: string | null }) {
  return namespace ? <NamespaceName name={namespace} /> : <span>Organization</span>;
}

/** The first line of a text, or `null` for none. */
export function firstLine(text: string | null | undefined): string | null {
  const line = text
    ?.split("\n")
    .map((l) => l.trim())
    .find(Boolean);
  return line || null;
}
