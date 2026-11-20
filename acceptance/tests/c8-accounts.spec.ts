/**
 * Criterion 8 (`archi/09-roadmap.md`, "0.1 is done when"): someone signs in to the dashboard with
 * a code by email, creates an organization, invites a member with a role, and creates an API key;
 * each endpoint refuses a caller without its permission.
 *
 * Against WorkOS Staging (`WORKOS_API_KEY`, `WORKOS_CLIENT_ID`), and skipped, saying so, without
 * them. The dashboard is the one `rewind server` serves, in Chromium, for the owner and, in a
 * browser of their own, the invitee. The test reads no mailbox: WorkOS's Create Magic Auth answers
 * a valid code of the email, which the test types where the dashboard asks for the code it had
 * WorkOS send, and the invitation's link is the dashboard with its token, read from WorkOS. Addresses are `@example.net` only (WorkOS's demo organization holds
 * `example.com`). Everything the test created in WorkOS is removed at the end, even after a
 * failure.
 */
import { randomBytes } from "node:crypto";
import { mkdtempSync } from "node:fs";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";
import { Rewind } from "@rewind.codes/sdk";
import { type Server, startServer } from "../support/deployment.js";
import { resultDir } from "../support/paths.js";
import { type Created, NO_STAGING, stagingCredentials, WorkOsStaging } from "../support/workos.js";

/** A run id no run has, and ids of WorkOS's shape it never gave. */
const UNKNOWN_RUN = "01930000-0000-7000-8000-00000000abcd";
const UNKNOWN_USER = "user_01JZZZZZZZZZZZZZZZZZZZZZZZ";
const UNKNOWN_INVITATION = "invitation_01JZZZZZZZZZZZZZZZZZZZZZZZ";

/**
 * One request per permission of the catalog, each needing that permission (06's table). Allowed,
 * each answers without changing anything that matters here: unknown ids are not found, an empty
 * toolset is registered, an empty change of the organization changes nothing.
 */
const PROBES: { permission: string; method: string; path: string; body?: unknown }[] = [
  { permission: "runs:read", method: "GET", path: "/v1/runs" },
  { permission: "runs:start", method: "POST", path: "/v1/check", body: { script: "export default async function main() { return 1; }" } },
  { permission: "runs:control", method: "POST", path: `/v1/runs/${UNKNOWN_RUN}/cancel`, body: {} },
  { permission: "runs:quarantine", method: "POST", path: `/v1/runs/${UNKNOWN_RUN}/release` },
  { permission: "runs:delete", method: "DELETE", path: `/v1/runs/${UNKNOWN_RUN}` },
  { permission: "toolsets:read", method: "GET", path: "/v1/toolsets" },
  { permission: "toolsets:write", method: "POST", path: "/v1/toolsets", body: {} },
  { permission: "credentials:write", method: "PUT", path: "/v1/toolsets/unknown/credentials", body: { servers: {} } },
  { permission: "tasks:work", method: "POST", path: "/v1/tasks/unknown/heartbeat" },
  { permission: "tasks:approve", method: "POST", path: "/v1/tasks/unknown/approve" },
  { permission: "subscriptions:read", method: "GET", path: "/v1/subscriptions" },
  { permission: "subscriptions:write", method: "DELETE", path: "/v1/subscriptions/unknown" },
  { permission: "members:read", method: "GET", path: "/v1/organization/members" },
  { permission: "members:invite", method: "POST", path: `/v1/organization/invitations/${UNKNOWN_INVITATION}/resend` },
  { permission: "members:manage", method: "PUT", path: `/v1/organization/members/${UNKNOWN_USER}/grant`, body: { role: "reader" } },
  { permission: "roles:read", method: "GET", path: "/v1/roles" },
  { permission: "roles:write", method: "DELETE", path: "/v1/roles/unknown-role" },
  { permission: "api_keys:read", method: "GET", path: "/v1/api-keys" },
  { permission: "api_keys:write", method: "DELETE", path: "/v1/api-keys/unknown" },
  { permission: "organization:write", method: "PATCH", path: "/v1/organization", body: {} },
  { permission: "namespaces:read", method: "GET", path: "/v1/namespaces" },
  { permission: "namespaces:write", method: "DELETE", path: "/v1/namespaces/unknown-namespace" },
  { permission: "tokens:write", method: "DELETE", path: "/v1/organization/token-keys/rk_unknown" },
];

interface Answer {
  status: number;
  body: { detail?: string; [key: string]: unknown };
}

/** A request to `/v1` with a Bearer token: a key, or a member's access token. */
async function call(server: Server, token: string, method: string, path: string, body?: unknown): Promise<Answer> {
  const response = await fetch(`${server.url}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${token}`,
      ...(body === undefined ? {} : { "content-type": "application/json" }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text === "" ? {} : (JSON.parse(text) as Answer["body"]) };
}

interface Whoami {
  tenant?: string;
  principal?: { type?: string; id?: string } | string;
  grant?: { role: string; allowedLabels?: Record<string, string> };
  permissions: string[];
}

/**
 * The caller holds `held` and nothing else: each probe of a permission it lacks is refused (403,
 * naming the permission), each other one is answered.
 */
async function expectPermissions(server: Server, token: string, who: string, held: string[]) {
  for (const probe of PROBES) {
    const answer = await call(server, token, probe.method, probe.path, probe.body);
    const what = `${who}: ${probe.method} ${probe.path} (${probe.permission}): ${answer.status} ${answer.body.detail ?? ""}`;
    if (held.includes(probe.permission)) {
      expect(answer.status, what).not.toBe(401);
      expect(answer.status, what).not.toBe(403);
    } else {
      expect(answer.status, what).toBe(403);
      expect(answer.body.detail, what).toContain(`\`${probe.permission}\``);
    }
  }
}

const credentials = stagingCredentials();

test.describe("criterion 8: accounts, organizations and access", () => {
  test.skip(credentials === undefined, NO_STAGING);

  const tag = randomBytes(4).toString("hex");
  const owner = `rewind-c8-${tag}-owner@example.net`;
  const invitee = `rewind-c8-${tag}-invitee@example.net`;
  const organizationName = `Rewind acceptance c8 ${tag}`;
  const created: Created = { emails: [owner, invitee], users: [], organizations: [], organizationPrefix: organizationName };
  let workos: WorkOsStaging;
  let server: Server;

  test.beforeAll(async () => {
    if (!credentials) return;
    workos = new WorkOsStaging(credentials.apiKey);
    server = await startServer(mkdtempSync(join(resultDir("c8-accounts"), "server-")), { workos: credentials });
  });

  test.afterAll(async () => {
    await server?.stop();
    if (!workos) return;
    const failures = await workos.cleanUp(created);
    expect(failures, "everything the test created in WorkOS Staging is removed").toEqual([]);
  });

  /** Opens the Organization view, from the account menu. */
  async function openOrganization(page: Page) {
    await page.getByLabel(/^Account:/).click();
    await page.getByRole("menuitem", { name: "Organization" }).click();
  }

  /** Opens a part of the Organization view, in its sidebar. */
  async function openSection(page: Page, name: string) {
    await page.getByRole("navigation", { name: "Organization" }).getByRole("link", { name, exact: true }).click();
  }

  test("someone signs in with a code, creates an organization, invites a member and creates a key; each endpoint refuses a caller without its permission", async ({
    page,
    browser,
  }) => {
    const origin = server.origin;

    await test.step("sign in with a code by email", async () => {
      await page.goto(`${origin}/`);
      const signIn = page.getByRole("region", { name: "Sign in" });
      await expect(signIn.getByRole("heading", { name: "Sign in to Rewind" })).toBeVisible();
      await signIn.getByLabel("Email").fill(owner);
      await signIn.getByRole("button", { name: "Send me a code" }).click();
      const enter = page.getByRole("region", { name: "Enter the code" });
      await expect(enter.getByRole("heading", { name: "Check your email" })).toBeVisible();
      await expect(enter).toContainText(owner);
      // The code the email would hold: one that WorkOS answers for this email.
      const { code, user } = await workos.magicCode(owner);
      created.users.push(user);
      await enter.getByLabel("Code", { exact: true }).fill(code);
    });

    let organization = "";
    await test.step("create an organization in the onboarding", async () => {
      await expect(page.getByRole("heading", { name: `Welcome, ${owner}` })).toBeVisible();
      await expect(page.getByText(`No invitation is waiting for ${owner}.`)).toBeVisible();
      const create = page.getByRole("region", { name: "Create an organization" });
      await create.getByLabel("Name").fill(organizationName);
      await create.getByRole("button", { name: "Create", exact: true }).click();
      // The session moved onto the new organization: its owner reads its runs.
      await expect(page).toHaveURL(/^[^#]*\/overview$/);
      await openOrganization(page);
      await expect(page).toHaveURL(/^[^#]*\/organization\/members/);
      await expect(page.getByRole("region", { name: "Organization" })).toContainText(organizationName);
      await expect(page.getByRole("heading", { level: 1, name: "Members" })).toBeVisible();
      organization = (await page.locator(".org-card .digest").textContent())?.trim() ?? "";
      expect(organization).toMatch(/^org_/);
      created.organizations.push(organization);
      const me = page.locator("table.grid tbody tr", { hasText: owner });
      await expect(me.locator(".tag", { hasText: /^owner$/ })).toBeVisible();
      await expect(me.locator(".tag", { hasText: /^active$/ })).toBeVisible();
    });

    await test.step("invite a member with the role reader", async () => {
      await openSection(page, "Invitations");
      const form = page.getByRole("form", { name: "Invite someone" });
      await form.getByLabel("Email").fill(invitee);
      await form.getByLabel("Role").selectOption("reader");
      await form.getByRole("button", { name: "Send the invitation" }).click();
      await expect(page.getByRole("status").filter({ hasText: `An invitation was sent to ${invitee}.` })).toBeVisible();
      const row = page.locator("table.grid tbody tr", { hasText: invitee });
      await expect(row.locator(".tag", { hasText: /^reader$/ })).toBeVisible();
      await expect(row.locator(".tag", { hasText: /^pending$/ })).toBeVisible();
    });

    let key = "";
    await test.step("create an API key with the role harness, its secret shown once", async () => {
      await openSection(page, "API keys");
      await page.getByRole("button", { name: "New key" }).click();
      const form = page.getByRole("form", { name: "New API key" });
      await form.getByLabel("Name").fill("acceptance harness");
      await form.getByLabel("Role").selectOption("harness");
      await form.getByRole("button", { name: "Create the key" }).click();
      const secret = page.getByRole("region", { name: "The new key" });
      await expect(secret.getByRole("heading", { name: "The key acceptance harness was created" })).toBeVisible();
      key = (await secret.getByTestId("secret").textContent())?.trim() ?? "";
      expect(key).toMatch(/^rewind_sk_/);
      await secret.getByRole("button", { name: "I have copied it" }).click();
      await expect(page.getByTestId("secret")).toHaveCount(0);
      const row = page.locator("table.grid tbody tr", { hasText: "acceptance harness" });
      await expect(row.locator(".tag", { hasText: /^harness$/ })).toBeVisible();
      await expect(row.locator(".tag", { hasText: /^active$/ })).toBeVisible();
      // Its creator, by name.
      await expect(row).toContainText(owner);
    });

    await test.step("the key works within its role, and is refused, naming the permission, outside it", async () => {
      const whoami = (await call(server, key, "GET", "/v1/whoami")).body as unknown as Whoami;
      expect(whoami.tenant).toBe(organization);
      expect(whoami.grant?.role).toBe("harness");
      // Every permission of the catalog is probed.
      const catalog = (await call(server, key, "GET", "/v1/permissions")).body as unknown as {
        permissions: { permission: string }[];
      };
      expect(catalog.permissions.map((p) => p.permission).sort()).toEqual(PROBES.map((p) => p.permission).sort());
      // A run started with the key, in the organization, for one of its end users.
      const harness = new Rewind({ url: server.url, apiKey: key }).namespace("c8-user");
      const started = await harness.runs.start({
        script: "export default async function main() { return { answer: 42 }; }",
        labels: { suite: "c8" },
      });
      const run = await harness.runs.get(started.id, { wait: "60s" });
      expect(run.status).toBe("completed");
      expect(run.output?.value).toEqual({ answer: 42 });
      expect(whoami.permissions.sort()).toEqual(
        [
          "runs:read",
          "runs:start",
          "runs:control",
          "runs:quarantine",
          "runs:delete",
          "toolsets:read",
          "toolsets:write",
          "credentials:write",
          "tasks:work",
          "tasks:approve",
          "subscriptions:read",
          "subscriptions:write",
          "members:read",
          "roles:read",
          "namespaces:read",
          "namespaces:write",
          "tokens:write",
        ].sort(),
      );
      await expectPermissions(server, key, "the harness key", whoami.permissions);
      // The owner sees the key's run in the dashboard, once the read model has it.
      await expect
        .poll(async () => ((await call(server, key, "GET", "/v1/runs?q=%40suite%3Ac8")).body.runs as unknown[]).length)
        .toBe(1);
      await page.goto(`${origin}/runs`);
      await expect(page.locator(`table.grid tbody tr .run-id[title="${started.id}"]`)).toBeVisible();
    });

    let inviteeToken = "";
    await test.step("the invitee opens the invitation's link and signs in with a code: their grant is reader", async () => {
      // The link of the invitation's email: the dashboard with the invitation's token.
      const invitation = await workos.invitation(organization, invitee);
      const context = await browser.newContext();
      try {
        const other = await context.newPage();
        // The access token the dashboard sends to /v1, which it keeps in memory only.
        other.on("request", (request) => {
          const authorization = request.headers().authorization;
          if (request.url().includes("/v1/") && authorization?.startsWith("Bearer ")) inviteeToken = authorization.slice(7);
        });
        await other.goto(`${origin}/?invitation_token=${encodeURIComponent(invitation.token)}`);
        const signIn = other.getByRole("region", { name: "Sign in" });
        await expect(signIn.getByRole("status")).toHaveText("You were invited to an organization: sign in to join it.");
        await signIn.getByLabel("Email").fill(invitee);
        await signIn.getByRole("button", { name: "Send me a code" }).click();
        const enter = other.getByRole("region", { name: "Enter the code" });
        await expect(enter.getByRole("heading", { name: "Check your email" })).toBeVisible();
        // Sending the code accepts nothing yet: the code does.
        expect(await workos.invitationState(invitation.id)).toBe("pending");
        const { code, user } = await workos.magicCode(invitee);
        created.users.push(user);
        await enter.getByLabel("Code", { exact: true }).fill(code);
        // In the organization at once, as a reader: the runs, and of the organization what reading
        // members and roles shows.
        await expect(other).toHaveURL(/^[^#]*\/overview$/);
        expect(other.url()).not.toContain("invitation_token");
        expect(await workos.invitationState(invitation.id)).toBe("accepted");
        await openOrganization(other);
        await expect(other.getByRole("region", { name: "Organization" })).toContainText(organizationName);
        const sections = other.getByRole("navigation", { name: "Organization" }).getByRole("link");
        await expect(sections).toHaveText(["Members", "Invitations", "Roles"]);
        await expect(other.locator("table.grid tbody tr", { hasText: invitee }).locator(".tag", { hasText: /^reader$/ })).toBeVisible();
        await expect.poll(() => inviteeToken).not.toBe("");
      } finally {
        await context.close();
      }

      const whoami = (await call(server, inviteeToken, "GET", "/v1/whoami")).body as unknown as Whoami;
      expect(whoami.tenant).toBe(organization);
      expect(whoami.grant?.role).toBe("reader");
      expect(whoami.permissions.sort()).toEqual(
        ["runs:read", "toolsets:read", "subscriptions:read", "members:read", "roles:read", "namespaces:read"].sort(),
      );
      await expectPermissions(server, inviteeToken, "the invitee, a reader", whoami.permissions);
      // It reads the organization's runs, the key's among them.
      const runs = (await call(server, inviteeToken, "GET", "/v1/runs?q=%40suite%3Ac8")).body as unknown as { runs: unknown[] };
      expect(runs.runs).toHaveLength(1);
    });

    await test.step("the owner sees the invitee as a reader, and the invitation accepted", async () => {
      await page.goto(`${origin}/organization/members`);
      const row = page.locator("table.grid tbody tr", { hasText: invitee });
      await expect(row.locator(".tag", { hasText: /^reader$/ })).toBeVisible();
      await openSection(page, "Invitations");
      const invitation = page.locator("table.grid tbody tr", { hasText: invitee });
      await expect(invitation.locator(".tag", { hasText: /^accepted$/ })).toBeVisible();
    });

    await test.step("removing the invitee cuts its access at its next request", async () => {
      await openSection(page, "Members");
      const row = page.locator("table.grid tbody tr", { hasText: invitee });
      await row.getByRole("button", { name: "Remove" }).click();
      await row.getByRole("group", { name: `Remove ${invitee}?` }).getByRole("button", { name: "Yes" }).click();
      await expect(page.locator("table.grid tbody tr", { hasText: invitee })).toHaveCount(0);
      // The same access token, still valid for WorkOS: no grant any more, so no permission.
      const refused = await call(server, inviteeToken, "GET", "/v1/runs");
      expect(refused.status).toBe(403);
      expect(refused.body.detail).toContain("no grant");
      expect(refused.body.detail).toContain("`runs:read`");
      const whoami = (await call(server, inviteeToken, "GET", "/v1/whoami")).body as unknown as Whoami;
      expect(whoami.grant).toBeUndefined();
      expect(whoami.permissions).toEqual([]);
    });
  });
});
