import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Labels, Permission } from "../../model/types";
import { confirmInDialog, renderApp, signedIn } from "../../test/app";

/** Gives Grace, in Acme, a role made of these permissions, and a grant limited to `labels`. */
function graceWith(permissions: Permission[], labels: Labels = {}) {
  const world = signedIn("grace@example.net");
  world.access.roles.get("org_01ACME")?.push({
    name: "custom",
    builtIn: false,
    description: "A role for the tests.",
    permissions,
  });
  world.access.grants
    .get("org_01ACME")
    ?.set("user:user_01GRACE", { role: "custom", allowedLabels: labels });
  return world;
}

/** The row of a secret, by its reference (`org/slack`). */
function secretRow(reference: string, within_: HTMLElement = document.body): HTMLElement {
  const table = within(within_).getByRole("table");
  const found = within(table)
    .getAllByRole("row")
    .find((r) => within(r).queryByText(reference) !== null);
  if (!found) throw new Error(`no row ${reference}`);
  return found;
}

async function listed(reference: string) {
  await waitFor(() => expect(screen.getByText(reference)).toBeInTheDocument());
}

describe("the Secrets view", () => {
  it("lists the organization's secrets: labels, who wrote them, reads, last read, what attaches them", async () => {
    renderApp("/secrets", { world: signedIn("ada@example.net", "org_01ACME") });
    await listed("org/slack");
    expect(screen.getByRole("link", { name: "Secrets" })).toHaveAttribute("aria-current", "page");
    // Only the organization's: the end users' are on their namespace's page.
    expect(screen.queryByText("ns/github")).toBeNull();

    const crm = secretRow("org/crm");
    expect(within(crm).getByText("prod")).toBeInTheDocument();
    expect(within(crm).getByText("Grace Hopper")).toBeInTheDocument();
    expect(within(crm).getByRole("img", { name: /Reads of org\/crm per day/ })).toBeInTheDocument();

    // The workflows whose latest version attaches it, and the active runs.
    const slack = secretRow("org/slack");
    expect(await within(slack).findByRole("link", { name: "triage" })).toHaveAttribute(
      "href",
      expect.stringMatching(/^\/testing\/triage\?version=/),
    );
    expect(within(slack).getByRole("link", { name: "digest" })).toBeInTheDocument();
    expect(within(slack).getByRole("button", { name: /active runs?/ })).toBeInTheDocument();
    // Its last read links to the run that read it.
    expect(within(slack).getByTitle("The run that read it last")).toHaveAttribute(
      "href",
      expect.stringMatching(/^\/runs\/[^/]+\/[^/]+\/[0-9a-z]{12,25}$/),
    );

    // Read only by a toolset's registration, to list its server's tools: no run to link to.
    const notion = secretRow("org/notion");
    expect(within(notion).getByText("at a toolset's registration")).toBeInTheDocument();
    expect(within(notion).queryByTitle("The run that read it last")).toBeNull();

    // Never read, attached by nothing.
    const zendesk = secretRow("org/zendesk");
    expect(within(zendesk).getByText("never")).toBeInTheDocument();
    expect(await within(zendesk).findByText("nothing active")).toBeInTheDocument();
    // No value anywhere.
    expect(document.body.textContent).not.toMatch(/zendesk-[0-9a-f]{12}/);
  });

  it("creates a secret: its value sent once, in a password field, and never shown", async () => {
    const user = userEvent.setup();
    const { callsTo, world } = renderApp("/secrets", {
      world: signedIn("ada@example.net", "org_01ACME"),
    });
    await listed("org/slack");
    await user.click(screen.getByRole("button", { name: /new secret/i }));
    const form = screen.getByRole("form", { name: "New secret" });
    expect(screen.getByRole("dialog")).toContainElement(form);
    const create = within(form).getByRole("button", { name: "Create the secret" });
    expect(create).toBeDisabled();

    await user.type(within(form).getByLabelText("Name"), "Linear");
    expect(within(form).getByText("Start with a lowercase letter.")).toBeInTheDocument();
    await user.clear(within(form).getByLabelText("Name"));
    await user.type(within(form).getByLabelText("Name"), "linear");
    const value = within(form).getByLabelText("Value");
    expect(value).toHaveAttribute("type", "password");
    await user.type(value, "lin_api_123");
    await user.type(within(form).getByLabelText("Labels"), "env:prod");
    await user.click(create);

    expect(await screen.findByRole("status")).toHaveTextContent(
      "The secret org/linear was created. Its value is not shown again.",
    );
    const [put] = callsTo("/v1/secrets/linear", "PUT");
    expect(JSON.parse((await put?.body) ?? "")).toEqual({
      value: "lin_api_123",
      labels: { env: "prod" },
    });
    await listed("org/linear");
    expect(within(secretRow("org/linear")).getByText("prod")).toBeInTheDocument();
    expect(document.body.textContent).not.toContain("lin_api_123");
    // No answer read back holds it either.
    const lists = callsTo("/v1/secrets", "GET");
    expect(lists.length).toBeGreaterThan(1);
    const stored = world.secrets
      .get("org_01ACME")
      ?.secrets.find((s) => s.namespace === null && s.name === "linear");
    expect(stored?.value).toBe("lin_api_123");
  });

  it("writes a new value of a secret, keeping its labels", async () => {
    const user = userEvent.setup();
    const { callsTo } = renderApp("/secrets", { world: signedIn("ada@example.net", "org_01ACME") });
    await listed("org/crm");
    await user.click(
      within(secretRow("org/crm")).getByRole("button", { name: "Write a new value of org/crm" }),
    );
    const form = screen.getByRole("form", { name: "New value of org/crm" });
    expect(within(form).queryByLabelText("Name")).toBeNull();
    expect(within(form).getByLabelText("Labels")).toHaveValue("env:prod");
    await user.type(within(form).getByLabelText("Value"), "crm_rotated");
    await user.click(within(form).getByRole("button", { name: "Replace the value" }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "The value of org/crm was replaced: its parked calls are sent again.",
    );
    const [put] = callsTo("/v1/secrets/crm", "PUT");
    expect(JSON.parse((await put?.body) ?? "")).toEqual({
      value: "crm_rotated",
      labels: { env: "prod" },
    });
  });

  it("deletes a secret once the dialog has said what it would stop", async () => {
    const user = userEvent.setup();
    const { callsTo } = renderApp("/secrets", { world: signedIn("ada@example.net", "org_01ACME") });
    await listed("org/slack");
    await user.click(
      within(secretRow("org/slack")).getByRole("button", { name: "Delete org/slack" }),
    );
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Delete org/slack?")).toBeInTheDocument();
    const impact = await within(dialog).findByTestId("impact");
    // The usage endpoint was read before anything was deleted.
    expect(callsTo("/v1/secrets/slack/usage", "GET").length).toBeGreaterThan(0);
    expect(impact).toHaveTextContent(/active runs? attach it: their next call .* is parked/);
    expect(impact).toHaveTextContent(/The latest version of 2 workflows attaches it/);
    expect(within(impact).getByText("triage")).toBeInTheDocument();
    expect(within(impact).getByText("digest")).toBeInTheDocument();
    expect(within(impact).getAllByRole("link").length).toBeGreaterThan(0);
    expect(callsTo("/v1/secrets/slack", "DELETE")).toHaveLength(0);

    await confirmInDialog(user);
    expect(await screen.findByRole("status")).toHaveTextContent(
      "The secret org/slack was deleted.",
    );
    expect(callsTo("/v1/secrets/slack", "DELETE")).toHaveLength(1);
    await waitFor(() => expect(screen.queryByText("org/slack")).toBeNull());
  });

  it("says when a deletion stops nothing", async () => {
    const user = userEvent.setup();
    renderApp("/secrets", { world: signedIn("ada@example.net", "org_01ACME") });
    await listed("org/zendesk");
    await user.click(
      within(secretRow("org/zendesk")).getByRole("button", { name: "Delete org/zendesk" }),
    );
    const impact = await screen.findByTestId("impact");
    expect(impact).toHaveTextContent("No active run and no workflow attaches it.");
    expect(impact).toHaveTextContent("Read 0 times in 30 days");
  });

  it("shows the secrets to secrets:read, and their writes to secrets:write only", async () => {
    renderApp("/secrets", { world: graceWith(["runs:read", "secrets:read"]) });
    await listed("org/github");
    expect(screen.queryByRole("button", { name: /new secret/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /Delete org\// })).toBeNull();
    expect(screen.queryByRole("columnheader", { name: "Actions" })).toBeNull();
  });

  it("shows a grant limited to labels only the secrets carrying them, and gives what it writes its labels", async () => {
    const user = userEvent.setup();
    const { callsTo } = renderApp("/secrets", {
      world: graceWith(["runs:read", "secrets:read", "secrets:write"], { env: "prod" }),
    });
    await listed("org/crm");
    expect(screen.getByText("org/zendesk")).toBeInTheDocument();
    expect(screen.queryByText("org/github")).toBeNull();
    await user.click(screen.getByRole("button", { name: /new secret/i }));
    const form = screen.getByRole("form", { name: "New secret" });
    expect(within(form).getByLabelText("Labels")).toHaveValue("env:prod");
    expect(
      within(form).getByText(/Your grant is limited to env:prod: the secret takes them/),
    ).toBeInTheDocument();
    await user.type(within(form).getByLabelText("Name"), "stripe");
    await user.type(within(form).getByLabelText("Value"), "sk_live");
    await user.clear(within(form).getByLabelText("Labels"));
    await user.click(within(form).getByRole("button", { name: "Create the secret" }));
    await screen.findByRole("status");
    const [put] = callsTo("/v1/secrets/stripe", "PUT");
    expect(JSON.parse((await put?.body) ?? "")).toEqual({
      value: "sk_live",
      labels: { env: "prod" },
    });
  });

  it("tells a member without secrets:read why, and keeps the view out of the navigation", async () => {
    renderApp("/secrets", { world: signedIn("linus@example.net") });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      /You cannot see the secrets in Acme.*secrets:read/,
    );
    expect(screen.queryByRole("link", { name: "Secrets" })).toBeNull();
  });
});

describe("a namespace's secrets", () => {
  it("lists the end user's own secrets in its scope, and writes and deletes them", async () => {
    const user = userEvent.setup();
    const { callsTo } = renderApp("/ns/alice/secrets");
    const section = await screen.findByRole("region", { name: "Its secrets" });
    await waitFor(() => expect(within(section).getByText("ns/github")).toBeInTheDocument());
    const github = secretRow("ns/github", section);
    expect(within(github).getByText(/the token of/)).toBeInTheDocument();
    // The latest version of triage attaches ns/github, for every namespace.
    expect(await within(github).findByRole("link", { name: "triage" })).toBeInTheDocument();
    // An end user's secret has no labels.
    expect(within(section).queryByRole("columnheader", { name: "Labels" })).toBeNull();

    await user.click(within(section).getByRole("button", { name: /new secret/i }));
    const form = screen.getByRole("form", { name: "New secret" });
    expect(within(form).queryByLabelText("Labels")).toBeNull();
    await user.type(within(form).getByLabelText("Name"), "linear");
    await user.type(within(form).getByLabelText("Value"), "lin_alice");
    await user.click(within(form).getByRole("button", { name: "Create the secret" }));
    await waitFor(() => expect(within(section).getByText("ns/linear")).toBeInTheDocument());
    const [put] = callsTo("/v1/namespaces/alice/secrets/linear", "PUT");
    expect(JSON.parse((await put?.body) ?? "")).toEqual({ value: "lin_alice" });

    await user.click(
      within(secretRow("ns/github", section)).getByRole("button", { name: "Delete ns/github" }),
    );
    const impact = await screen.findByTestId("impact");
    expect(impact).toHaveTextContent(/their runs in this namespace are refused at start/);
    await confirmInDialog(user);
    await waitFor(() => expect(within(section).queryByText("ns/github")).toBeNull());
    expect(callsTo("/v1/namespaces/alice/secrets/github", "DELETE")).toHaveLength(1);
  });

  it("never offers a grant limited to labels to write an end user's secret", async () => {
    const user = userEvent.setup();
    renderApp("/ns/alice/secrets", {
      world: graceWith(["runs:read", "namespaces:read", "secrets:read", "secrets:write"], {
        agent: "triage",
      }),
    });
    const section = await screen.findByRole("region", { name: "Its secrets" });
    await waitFor(() => expect(within(section).getByText("ns/github")).toBeInTheDocument());
    expect(within(section).queryByRole("button", { name: /new secret/i })).toBeNull();
    // Why, in the card of the (i) beside the page's title.
    await user.click(screen.getByRole("button", { name: "About Secrets" }));
    expect(await screen.findByText(/Your grant is limited to labels/)).toBeInTheDocument();
  });

  it("lists the organization's secrets after its own, to read only", async () => {
    renderApp("/ns/alice/secrets");
    const org = await screen.findByRole("region", { name: "The organization's secrets" });
    await waitFor(() => expect(within(org).getByText("org/slack")).toBeInTheDocument());
    expect(within(org).queryByRole("button", { name: /new secret/i })).toBeNull();
    expect(within(org).queryByRole("button", { name: /^Delete / })).toBeNull();
  });

  it("is not shown without secrets:read", async () => {
    renderApp("/ns/alice/usage", { world: graceWith(["runs:read", "namespaces:read"]) });
    await screen.findByRole("img", { name: /^Cumulative CPU time of alice/ });
    const nav = screen.getByRole("navigation", { name: "Views" });
    expect(within(nav).queryByRole("link", { name: "Secrets" })).toBeNull();
  });
});

describe("the signing keys", () => {
  it("shows the keys and the address of their public keys, and rotates the key", async () => {
    const user = userEvent.setup();
    const { callsTo } = renderApp("/organization/signing-keys", {
      world: signedIn("ada@example.net", "org_01ACME"),
    });
    const section = await screen.findByRole("region", { name: "Signing keys" });
    expect(await within(section).findByTestId("issuer")).toHaveTextContent(
      /\/organizations\/org_01ACME$/,
    );
    expect(within(section).getByTestId("jwks-url")).toHaveTextContent(
      /\/organizations\/org_01ACME\/jwks\.json$/,
    );
    expect(within(section).getByRole("button", { name: "Copy the issuer" })).toBeInTheDocument();
    const table = within(section).getByRole("table");
    expect(within(table).getAllByText("signing")).toHaveLength(1);
    expect(within(table).getByText(/retired .* published until/)).toBeInTheDocument();

    await user.click(within(section).getByRole("button", { name: "Rotate the key" }));
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent(/stays published for 24 hours/);
    await confirmInDialog(user);
    await waitFor(() =>
      expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(4),
    );
    expect(callsTo("/v1/organization/signing-keys", "POST")).toHaveLength(1);
    expect(within(screen.getByRole("table")).getAllByText("signing")).toHaveLength(1);
  });

  it("lets secrets:read see them, and only organization:write rotate them", async () => {
    // Grace's role, harness, reads the secrets and does not administer the organization.
    renderApp("/organization/signing-keys", { world: signedIn("grace@example.net") });
    const section = await screen.findByRole("region", { name: "Signing keys" });
    await within(section).findByTestId("issuer");
    expect(within(section).queryByRole("button", { name: "Rotate the key" })).toBeNull();
  });
});

describe("a run's details", () => {
  it("name the secrets it attached, never a value", async () => {
    const world = signedIn("ada@example.net", "org_01ACME");
    const run = world.runs.find((r) => r.secrets?.github === "ns/github");
    if (!run) throw new Error("no run attaching ns/github");
    renderApp(`/runs/${run.id}`, { world });
    const term = await screen.findByText("Secrets", { selector: "dt" });
    const details = term.parentElement as HTMLElement;
    expect(within(details).getByRole("link", { name: "ns/github" })).toHaveAttribute(
      "href",
      `/ns/${run.namespace}/secrets`,
    );
    expect(within(details).getByRole("link", { name: "org/slack" })).toHaveAttribute(
      "href",
      "/secrets",
    );
  });
});
