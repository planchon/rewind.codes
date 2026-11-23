// The menus of the frame (archi/05, "One sidebar"): at the top of the sidebar, the organization
// picker (the member's organizations, the invitations waiting, a new organization); at its
// bottom, the account (the organization's parts, Connect Claude and the connections, the theme,
// the time zone, the server, the sign-out). Menus and dialogs are Base UI's, styled with the tokens.
import { Dialog } from "@base-ui/react/dialog";
import { Menu } from "@base-ui/react/menu";
import {
  Check,
  ChevronRight,
  ChevronsUpDown,
  Ellipsis,
  KeyRound,
  LogOut,
  Monitor,
  Moon,
  Plug,
  PlugZap,
  Plus,
  SlidersHorizontal,
  Sun,
} from "lucide-react";
import { type FormEvent, useContext, useState } from "react";
import { useNavigate } from "react-router";
import { ErrorNote } from "../components/ErrorNote";
import { displayName } from "../model/access";
import type { Organization, User } from "../model/types";
import { AccessContext, useAccess } from "../state/access";
import { useJoinOrCreate, useMyInvitations } from "../state/organization";
import { maskKey, useSession, useSignOut, useSwitchOrganization } from "../state/session";
import { Avatar, Mark } from "./Brand";
import { useServer } from "./Gate";
import { type ThemeChoice, useSettings } from "./settings";

/** An organization's mark: a gradient drawn from its id, as Vercel's teams have. */
export function OrgAvatar({
  organization,
  size = 20,
}: {
  organization: Organization;
  size?: number;
}) {
  let hash = 0;
  for (const c of organization.id) hash = (hash * 31 + c.charCodeAt(0)) >>> 0;
  const hue = hash % 360;
  return (
    <span
      className="org-avatar"
      aria-hidden="true"
      style={{
        width: size,
        height: size,
        background: `linear-gradient(135deg, hsl(${hue} 70% 55%), hsl(${(hue + 50) % 360} 80% 45%))`,
      }}
    />
  );
}

/** The scope's head: the mark, then the organization (or the key's tenant). */
function ScopeHead({ name, role }: { name: string; role?: string | null }) {
  return (
    <>
      <Mark />
      <span className="scope-slash" aria-hidden="true">
        /
      </span>
      <span className="scope-name">{name}</span>
      {role && <span className="badge gray">{role}</span>}
    </>
  );
}

function CreateOrganization({
  open,
  onOpenChange,
  onCreate,
  pending,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreate: (name: string) => void;
  pending: boolean;
}) {
  const [name, setName] = useState("");
  const submit = (e: FormEvent) => {
    e.preventDefault();
    const trimmed = name.trim();
    if (trimmed) onCreate(trimmed);
  };
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Backdrop className="dialog-backdrop" />
        <Dialog.Popup className="dialog">
          <form onSubmit={submit}>
            <div className="dialog-body">
              <Dialog.Title className="dialog-title">Create an organization</Dialog.Title>
              <Dialog.Description className="dialog-text">
                Runs, scripts and keys belong to an organization. You will be its owner.
              </Dialog.Description>
              <label className="field col">
                <span className="field-label">Name</span>
                <input
                  className="input"
                  value={name}
                  placeholder="Acme"
                  onChange={(e) => setName(e.target.value)}
                />
              </label>
            </div>
            <div className="dialog-foot">
              <Dialog.Close className="btn">Cancel</Dialog.Close>
              <button type="submit" className="btn primary" disabled={!name.trim() || pending}>
                Create
              </button>
            </div>
          </form>
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

/** The organization picker, at the top of the sidebar; for a key, its tenant. */
export function OrganizationSwitcher() {
  const access = useAccess();
  const { phase } = useSession();
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const [creating, setCreating] = useState(false);
  const invitations = useMyInvitations(open);
  // The new organization opens on its home page: a run of the previous one is not in it.
  const done = () => {
    setCreating(false);
    void navigate("/");
  };
  const { join, create } = useJoinOrCreate(done);
  const toSwitch = useSwitchOrganization(done);
  const current = access.organization;

  if (phase.kind !== "member" || !current) {
    const tenant = access.identity.tenant;
    return (
      <div className="scope static" title={tenant ? `The key's tenant: ${tenant}` : undefined}>
        <ScopeHead name={tenant ?? "Rewind"} />
      </div>
    );
  }

  const pending = toSwitch.pending || join.pending || create.pending;
  const error = toSwitch.error ?? join.error ?? create.error;
  const waiting = invitations.data ?? [];

  return (
    <>
      <Menu.Root open={open} onOpenChange={setOpen}>
        <Menu.Trigger
          className="scope"
          aria-label={`Organization: ${current.name}`}
          title="Switch organization"
        >
          <ScopeHead name={current.name} role={access.role} />
          <ChevronsUpDown size={14} className="scope-chevron" aria-hidden="true" />
        </Menu.Trigger>
        <Menu.Portal>
          <Menu.Positioner className="menu-positioner" side="bottom" align="start" sideOffset={6}>
            <Menu.Popup className="menu-popup scope-menu">
              <Menu.Group>
                <Menu.GroupLabel className="menu-label">Organizations</Menu.GroupLabel>
                {access.organizations.map((o) => (
                  <Menu.Item
                    key={o.id}
                    className="menu-item"
                    disabled={pending}
                    aria-current={o.id === current.id ? "true" : undefined}
                    onClick={() => {
                      if (o.id !== current.id) void toSwitch.run(o.id);
                    }}
                  >
                    <OrgAvatar organization={o} />
                    <span className="menu-item-text">{o.name}</span>
                    {o.id === current.id && (
                      <Check size={14} className="menu-item-end" aria-hidden="true" />
                    )}
                  </Menu.Item>
                ))}
              </Menu.Group>
              {waiting.length > 0 && (
                <>
                  <Menu.Separator className="menu-separator" />
                  <Menu.Group>
                    <Menu.GroupLabel className="menu-label">Invitations</Menu.GroupLabel>
                    {waiting.map((inv) => (
                      <Menu.Item
                        key={inv.id}
                        className="menu-item"
                        disabled={pending}
                        onClick={() => void join.run(inv)}
                      >
                        <Plus size={14} aria-hidden="true" />
                        <span className="menu-item-text">
                          Join {inv.organization?.name ?? "the organization"}
                        </span>
                      </Menu.Item>
                    ))}
                  </Menu.Group>
                </>
              )}
              <Menu.Separator className="menu-separator" />
              <Menu.Item className="menu-item" onClick={() => setCreating(true)}>
                <Plus size={14} aria-hidden="true" />
                <span className="menu-item-text">Create organization</span>
              </Menu.Item>
            </Menu.Popup>
          </Menu.Positioner>
        </Menu.Portal>
      </Menu.Root>
      {error ? <ErrorNote error={error} /> : null}
      <CreateOrganization
        open={creating}
        onOpenChange={setCreating}
        onCreate={(name) => void create.run(name)}
        pending={create.pending}
      />
    </>
  );
}

const THEMES: { value: ThemeChoice; label: string; Icon: typeof Sun }[] = [
  { value: "system", label: "System", Icon: Monitor },
  { value: "light", label: "Light", Icon: Sun },
  { value: "dark", label: "Dark", Icon: Moon },
];

/** The viewer's settings, inline in the account menu. */
function SettingsItems() {
  const { theme, setTheme, zone, setZone } = useSettings();
  return (
    <>
      <div className="menu-inline">
        <span id="theme-label">Theme</span>
        <Menu.RadioGroup
          className="seg small"
          aria-labelledby="theme-label"
          value={theme}
          onValueChange={(v) => setTheme(v as ThemeChoice)}
        >
          {THEMES.map(({ value, label, Icon }) => (
            <Menu.RadioItem
              key={value}
              value={value}
              closeOnClick={false}
              className="seg-item"
              aria-label={label}
              title={label}
            >
              <Icon size={13} aria-hidden="true" />
            </Menu.RadioItem>
          ))}
        </Menu.RadioGroup>
      </div>
      <div className="menu-inline">
        <span id="zone-label">Time zone</span>
        <Menu.RadioGroup
          className="seg small"
          aria-labelledby="zone-label"
          value={zone}
          onValueChange={(v) => setZone(v === "local" ? "local" : "utc")}
        >
          <Menu.RadioItem value="utc" closeOnClick={false} className="seg-item">
            UTC
          </Menu.RadioItem>
          <Menu.RadioItem value="local" closeOnClick={false} className="seg-item">
            Local
          </Menu.RadioItem>
        </Menu.RadioGroup>
      </div>
    </>
  );
}

/**
 * The account, at the bottom of the sidebar (or of the onboarding): who is signed in, the
 * organization's parts, the settings, the sign-out.
 */
export function AccountMenu({
  user,
  server,
  onOrganization,
  onConnect,
  onConnections,
}: {
  user: User | null;
  server?: string | null;
  /** Opens the organization's parts; the onboarding has none. */
  onOrganization?: () => void;
  /** Opens Connect Claude. */
  onConnect?: () => void;
  /** Opens the member's connections (Account › Connections); a key has none. */
  onConnections?: () => void;
}) {
  const { phase, key } = useSession();
  // The onboarding has no access yet: a member without organization.
  const access = useContext(AccessContext);
  const out = useSignOut();
  const name = user ? displayName(user) : null;
  const organization = onOrganization && access?.sections.length ? onOrganization : null;
  return (
    <>
      <Menu.Root>
        <Menu.Trigger
          className="account"
          aria-label={name ? `Account: ${name}` : "Account: API key"}
          title={name ?? "The API key in use"}
        >
          {user ? (
            <Avatar user={user} size={20} />
          ) : (
            <span className="key-avatar" aria-hidden="true">
              <KeyRound size={12} />
            </span>
          )}
          <span className="account-name">
            {name ?? (key ? <span className="mono">{maskKey(key)}</span> : "API key")}
          </span>
          <Ellipsis size={16} className="account-end" aria-hidden="true" />
        </Menu.Trigger>
        <Menu.Portal>
          <Menu.Positioner className="menu-positioner" side="top" align="start" sideOffset={6}>
            <Menu.Popup className="menu-popup account-menu">
              <div className="menu-who">
                {user ? (
                  <>
                    <strong>{name}</strong>
                    <span>{user.email}</span>
                  </>
                ) : (
                  <>
                    <strong>API key</strong>
                    <span className="mono">{key ? maskKey(key) : "none"}</span>
                  </>
                )}
                {access?.organization && (
                  <span className="menu-who-role">
                    {access.role ? (
                      <>
                        <span className="badge gray">{access.role}</span> in{" "}
                        {access.organization.name}
                      </>
                    ) : (
                      <>No role in {access.organization.name}</>
                    )}
                  </span>
                )}
              </div>
              {(organization || onConnect || (user && onConnections)) && (
                <Menu.Separator className="menu-separator" />
              )}
              {organization && (
                <Menu.Item className="menu-item" onClick={organization}>
                  <SlidersHorizontal size={14} aria-hidden="true" />
                  <span className="menu-item-text">Organization</span>
                  <ChevronRight size={14} className="menu-item-end" aria-hidden="true" />
                </Menu.Item>
              )}
              {onConnect && (
                <Menu.Item className="menu-item" onClick={onConnect}>
                  <PlugZap size={14} aria-hidden="true" />
                  <span className="menu-item-text">Connect Claude</span>
                  <ChevronRight size={14} className="menu-item-end" aria-hidden="true" />
                </Menu.Item>
              )}
              {user && onConnections && (
                <Menu.Item className="menu-item" onClick={onConnections}>
                  <Plug size={14} aria-hidden="true" />
                  <span className="menu-item-text">Connections</span>
                  <ChevronRight size={14} className="menu-item-end" aria-hidden="true" />
                </Menu.Item>
              )}
              <Menu.Separator className="menu-separator" />
              <SettingsItems />
              {server && <div className="menu-meta mono">{server}</div>}
              <Menu.Separator className="menu-separator" />
              <Menu.Item
                className="menu-item"
                disabled={out.pending || phase.kind === "starting"}
                onClick={() => void out.run()}
              >
                <LogOut size={14} aria-hidden="true" />
                <span className="menu-item-text">Sign out</span>
              </Menu.Item>
            </Menu.Popup>
          </Menu.Positioner>
        </Menu.Portal>
      </Menu.Root>
      {out.error ? <ErrorNote error={out.error} title="Could not sign out" /> : null}
    </>
  );
}

/** The server's version and epochs, and the nodes of a deployment, as the account menu shows them. */
export function useServerLine(): string {
  const server = useServer();
  const epochs = server.epochs.join(", ");
  return [
    `rewind ${server.version}`,
    `epoch${server.epochs.length > 1 ? "s" : ""} ${epochs}`,
    server.embedded ? "embedded" : `${server.nodes} node${server.nodes === 1 ? "" : "s"}`,
    server.virtualTime ? "virtual time" : null,
  ]
    .filter(Boolean)
    .join(" · ");
}
