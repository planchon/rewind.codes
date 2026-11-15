/**
 * WorkOS Staging, as a test reaches it beside the dashboard: the code a person would read in their
 * mailbox, the invitation's link they would follow, and the removal of everything the test
 * created. The API key goes to WorkOS only: it is never printed, logged nor put in an error, and
 * these calls go through Node's `fetch`, outside Playwright's traces.
 */
import { setTimeout as delay } from "node:timers/promises";

const API = "https://api.workos.com";

/** WorkOS Staging's credentials, from `WORKOS_API_KEY` and `WORKOS_CLIENT_ID`. */
export interface StagingCredentials {
  apiKey: string;
  clientId: string;
}

/** The reason criterion 8 cannot run, when it cannot. */
export const NO_STAGING = "WORKOS_API_KEY and WORKOS_CLIENT_ID (WorkOS Staging) are not set";

/** The credentials of WorkOS Staging in the environment, if both are set. */
export function stagingCredentials(): StagingCredentials | undefined {
  const apiKey = process.env.WORKOS_API_KEY?.trim();
  const clientId = process.env.WORKOS_CLIENT_ID?.trim();
  return apiKey && clientId ? { apiKey, clientId } : undefined;
}

interface Invitation {
  id: string;
  email: string;
  state: string;
  token: string;
  organization_id: string | null;
}

/** What the test created: removed at the end, with what WorkOS lists for its emails. */
export interface Created {
  /** The emails of the test's people: their users, memberships and invitations. */
  emails: string[];
  /** The users the test saw WorkOS create. */
  users: string[];
  /** The organizations the test knows it created. */
  organizations: string[];
  /** Only organizations whose name starts with this are deleted: never WorkOS's demo one. */
  organizationPrefix: string;
}

/**
 * Calls `check` until it answers a value other than `undefined`, for about 30 s: WorkOS's lists
 * may lag a write by a few seconds.
 */
async function settled<T>(what: string, check: () => Promise<T | undefined>): Promise<T> {
  for (let tries = 0; ; tries++) {
    const value = await check();
    if (value !== undefined) return value;
    if (tries === 30) throw new Error(what);
    await delay(1_000);
  }
}

export class WorkOsStaging {
  constructor(private readonly apiKey: string) {}

  private async call<T>(method: string, path: string, body?: unknown): Promise<{ status: number; body: T }> {
    const response = await fetch(`${API}${path}`, {
      method,
      headers: {
        authorization: `Bearer ${this.apiKey}`,
        ...(body === undefined ? {} : { "content-type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    // A deletion answers 202 with the text `Accepted`.
    const json = response.headers.get("content-type")?.includes("json") ?? false;
    const parsed = json ? ((await response.json()) as unknown) : (await response.text(), undefined);
    if (!response.ok && response.status !== 404) {
      const problem = (parsed ?? {}) as { code?: string; error?: string; message?: string };
      // The path without its query; WorkOS's message never holds the API key.
      throw new Error(
        `WorkOS ${method} ${path.split("?")[0]}: ${response.status} ${problem.code ?? problem.error ?? ""} ${problem.message ?? ""}`.trim(),
      );
    }
    return { status: response.status, body: parsed as T };
  }

  /**
   * A 6-digit code for `email`, as WorkOS's Create Magic Auth answers it: the code the person
   * would read in the email the dashboard asked WorkOS to send. Any valid code of the email signs
   * in, whoever asked for it. With the user it is for.
   */
  async magicCode(email: string): Promise<{ code: string; user: string }> {
    const { body } = await this.call<{ code: string; user_id: string }>("POST", "/user_management/magic_auth", { email });
    if (!/^\d{6}$/.test(body.code)) throw new Error("WorkOS's Create Magic Auth answered no 6-digit code");
    return { code: body.code, user: body.user_id };
  }

  /** The pending invitation of `email` to `organization`: its token is the email's link. */
  async invitation(organization: string, email: string): Promise<Invitation> {
    const query = new URLSearchParams({ organization_id: organization, email });
    return settled(`no pending invitation of ${email} in WorkOS`, async () => {
      const { body } = await this.call<{ data: Invitation[] }>("GET", `/user_management/invitations?${query}`);
      return body.data.find((i) => i.email === email && i.state === "pending");
    });
  }

  /** The state of the invitation `id`: `pending`, `accepted`, `expired` or `revoked`. */
  async invitationState(id: string): Promise<string> {
    const { body } = await this.call<Invitation>("GET", `/user_management/invitations/${id}`);
    return body.state;
  }

  /** The users WorkOS lists for `email`. */
  private async usersOf(email: string): Promise<string[]> {
    const { body } = await this.call<{ data: { id: string; email: string }[] }>(
      "GET",
      `/user_management/users?${new URLSearchParams({ email })}`,
    );
    return body.data.filter((u) => u.email === email).map((u) => u.id);
  }

  /** Whether the user `id` exists. */
  private async userExists(id: string): Promise<boolean> {
    return (await this.call("GET", `/user_management/users/${id}`)).status !== 404;
  }

  /** The organizations `user` has a membership of. */
  private async organizationsOf(user: string): Promise<string[]> {
    const { body } = await this.call<{ data: { organization_id: string }[] }>(
      "GET",
      `/user_management/organization_memberships?${new URLSearchParams({ user_id: user, limit: "100" })}`,
    );
    return body.data.map((m) => m.organization_id);
  }

  /** The organization `id`, or `undefined` once deleted. */
  private async organization(id: string): Promise<{ id: string; name: string } | undefined> {
    const { status, body } = await this.call<{ id: string; name: string }>("GET", `/organizations/${id}`);
    return status === 404 ? undefined : body;
  }

  /**
   * Removes what the test created, even after a failure: the pending invitations of its emails
   * (revoked), its organizations (deleted with their memberships and invitations), its users.
   * Then checks that none is left. Answers what could not be removed.
   */
  async cleanUp(created: Created): Promise<string[]> {
    const failures: string[] = [];
    const attempt = async (what: string, action: () => Promise<unknown>) => {
      try {
        await action();
      } catch (error) {
        failures.push(`${what}: ${error instanceof Error ? error.message : String(error)}`);
      }
    };
    const users = new Set(created.users);
    const organizations = new Set(created.organizations);
    for (const email of created.emails) {
      await attempt(`find the users of ${email}`, async () => {
        for (const user of await this.usersOf(email)) users.add(user);
      });
      await attempt(`revoke the invitations of ${email}`, async () => {
        const { body } = await this.call<{ data: Invitation[] }>(
          "GET",
          `/user_management/invitations?${new URLSearchParams({ email })}`,
        );
        for (const invitation of body.data.filter((i) => i.email === email && i.state === "pending")) {
          await this.call("POST", `/user_management/invitations/${invitation.id}/revoke`);
        }
      });
    }
    for (const user of users) {
      await attempt(`find the organizations of ${user}`, async () => {
        for (const organization of await this.organizationsOf(user)) organizations.add(organization);
      });
    }
    for (const id of organizations) {
      await attempt(`delete the organization ${id}`, async () => {
        const organization = await this.organization(id);
        // Only the test's own: a membership of another organization is not the test's to delete.
        if (!organization?.name.startsWith(created.organizationPrefix)) return;
        await this.call("DELETE", `/organizations/${id}`);
      });
    }
    for (const id of users) {
      await attempt(`delete the user ${id}`, () => this.call("DELETE", `/user_management/users/${id}`));
    }
    // Nothing is left, once WorkOS's deletions (202) and lists have caught up.
    for (const id of users) {
      await attempt(`check that ${id} is gone`, () =>
        settled("the user is still in WorkOS", async () => ((await this.userExists(id)) ? undefined : true)),
      );
    }
    for (const email of created.emails) {
      await attempt(`check that ${email} is gone`, () =>
        settled("WorkOS still lists a user of it", async () => ((await this.usersOf(email)).length > 0 ? undefined : true)),
      );
    }
    for (const id of organizations) {
      await attempt(`check that ${id} is gone`, () =>
        settled("the organization is still in WorkOS", async () => {
          const organization = await this.organization(id);
          return organization?.name.startsWith(created.organizationPrefix) ? undefined : true;
        }),
      );
    }
    return failures;
  }
}
