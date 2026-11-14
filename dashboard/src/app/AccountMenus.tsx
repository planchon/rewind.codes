// The member's menus in the top bar: the organization switcher (with the invitations waiting and
// a new organization), and the user menu (who is signed in; sign out).
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Building2, Check, ChevronDown, LogOut, Plus } from "lucide-react";
import { type FormEvent, type RefObject, useContext, useEffect, useRef, useState } from "react";
import { MY_INVITATIONS, useMyInvitations } from "../api/organization";
import type { Invitation, User } from "../api/types";
import { ErrorNote } from "../components/ErrorNote";
import { displayName } from "../model/access";
import { AccessContext, useAccess } from "./access";
import { Avatar } from "./Brand";
import { useNavigate } from "./namespace";
import { useSession } from "./session";

/** Closes an open `<details>` menu on a click outside it, or on Escape. */
export function useDismiss(menu: RefObject<HTMLDetailsElement | null>): () => void {
  useEffect(() => {
    const onPointer = (e: PointerEvent) => {
      const el = menu.current;
      if (el?.open && e.target instanceof Node && !el.contains(e.target)) el.open = false;
    };
    const onKey = (e: KeyboardEvent) => {
      const el = menu.current;
      if (e.key === "Escape" && el?.open) {
        el.open = false;
        el.querySelector("summary")?.focus();
      }
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [menu]);
  return () => {
    if (menu.current) menu.current.open = false;
  };
}

/**
 * Joins an invitation's organization, or creates one, then switches the session to it. `then`
 * runs right after the switch, before the views of the new organization replace these ones.
 */
export function useJoinOrCreate(then: () => void = () => {}) {
  const { api, switchOrganization } = useSession();
  const queryClient = useQueryClient();
  const join = useMutation({
    mutationFn: async (invitation: Invitation) => {
      await api.acceptInvitation(invitation.id);
      const organization = invitation.organization?.id;
      if (!organization) {
        await queryClient.invalidateQueries({ queryKey: MY_INVITATIONS });
        return;
      }
      await switchOrganization(organization);
      then();
    },
  });
  const create = useMutation({
    mutationFn: async (name: string) => {
      const organization = await api.createOrganization(name);
      await switchOrganization(organization.id);
      then();
    },
  });
  return { join, create };
}

export function OrganizationSwitcher() {
  const access = useAccess();
  const { switchOrganization } = useSession();
  const navigate = useNavigate();
  const menu = useRef<HTMLDetailsElement>(null);
  const close = useDismiss(menu);
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const invitations = useMyInvitations(open);
  // The new organization opens on its home page: a run of the previous one is not in it.
  const done = () => {
    close();
    navigate("/");
  };
  const { join, create } = useJoinOrCreate(done);
  const current = access.organization;
  const toSwitch = useMutation({
    mutationFn: async (id: string) => {
      await switchOrganization(id);
      done();
    },
  });

  if (!current) return null;
  const pending = toSwitch.isPending || join.isPending || create.isPending;
  const error = toSwitch.error ?? join.error ?? create.error;
  const submit = (e: FormEvent) => {
    e.preventDefault();
    const trimmed = name.trim();
    if (trimmed) create.mutate(trimmed);
  };

  return (
    <details
      className="menu org-menu"
      ref={menu}
      onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}
    >
      <summary title="Switch organization" aria-label={`Organization: ${current.name}`}>
        <Building2 size={13} aria-hidden="true" />
        <span className="menu-current">{current.name}</span>
        <ChevronDown size={12} aria-hidden="true" />
      </summary>
      <div className="menu-pop">
        <div className="menu-label">Organizations</div>
        <ul aria-label="Your organizations">
          {access.organizations.map((o) => (
            <li key={o.id}>
              <button
                type="button"
                className="menu-item"
                aria-current={o.id === current.id ? "true" : undefined}
                disabled={pending || o.id === current.id}
                onClick={() => toSwitch.mutate(o.id)}
              >
                <span className="menu-check">
                  {o.id === current.id && <Check size={12} aria-hidden="true" />}
                </span>
                {o.name}
              </button>
            </li>
          ))}
        </ul>
        {(invitations.data ?? []).length > 0 && (
          <>
            <div className="menu-label">Invitations</div>
            <ul aria-label="Invitations waiting">
              {(invitations.data ?? []).map((inv) => (
                <li key={inv.id}>
                  <button
                    type="button"
                    className="menu-item"
                    disabled={pending}
                    onClick={() => join.mutate(inv)}
                  >
                    <span className="menu-check">
                      <Plus size={12} aria-hidden="true" />
                    </span>
                    Join {inv.organization?.name ?? "the organization"}
                  </button>
                </li>
              ))}
            </ul>
          </>
        )}
        <form className="menu-form" onSubmit={submit}>
          <label className="sr-only" htmlFor="new-organization">
            New organization's name
          </label>
          <input
            id="new-organization"
            className="input"
            placeholder="New organization"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <button type="submit" className="btn small" disabled={!name.trim() || pending}>
            Create
          </button>
        </form>
        {error ? <ErrorNote error={error} /> : null}
      </div>
    </details>
  );
}

export function UserMenu({ user }: { user: User }) {
  const { signOut } = useSession();
  // The onboarding has no access yet: a member without organization.
  const access = useContext(AccessContext);
  const menu = useRef<HTMLDetailsElement>(null);
  useDismiss(menu);
  const out = useMutation({ mutationFn: () => signOut() });
  const name = displayName(user);
  return (
    <details className="menu user-menu" ref={menu}>
      <summary aria-label={`Account: ${name}`} title={name}>
        <Avatar user={user} />
      </summary>
      <div className="menu-pop">
        <div className="user-card">
          <Avatar user={user} size={32} />
          <div className="user-card-text">
            <strong>{name}</strong>
            <span className="muted">{user.email}</span>
          </div>
        </div>
        {access?.organization && (
          <p className="menu-note">
            {access.role ? (
              <>
                Role <span className="tag">{access.role}</span> in {access.organization.name}
              </>
            ) : (
              <>No role in {access.organization.name}</>
            )}
          </p>
        )}
        <button
          type="button"
          className="menu-item"
          disabled={out.isPending}
          onClick={() => out.mutate()}
        >
          <span className="menu-check">
            <LogOut size={12} aria-hidden="true" />
          </span>
          Sign out
        </button>
        {out.error ? <ErrorNote error={out.error} title="Could not sign out" /> : null}
      </div>
    </details>
  );
}
