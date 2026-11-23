import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { createWorld, type World } from "../mocks/fixtures";
import { NOW, renderApp, signedIn } from "../test/app";

function withWorkos(): World {
  return createWorld({ now: NOW, workos: true });
}

/** The code the mocked WorkOS sent to the email. */
function codeFor(world: World, email: string): string {
  const code = world.access.codes.get(email);
  if (!code) throw new Error(`no code sent to ${email}`);
  return code;
}

async function sendCode(user: ReturnType<typeof userEvent.setup>, email: string) {
  await user.type(await screen.findByLabelText("Email"), email);
  await user.click(screen.getByRole("button", { name: /send me a code/i }));
  await screen.findByRole("heading", { name: "Check your email" });
}

describe("sign-in", () => {
  it("keeps the API key's gate when the server has no sign-in", async () => {
    const world = createWorld({ now: NOW, requireKey: "rewind_sk_good" });
    const { requested } = renderApp("/runs", { world });
    expect(await screen.findByLabelText("API key")).toBeInTheDocument();
    expect(requested("/auth/config")).toHaveLength(1);
    expect(requested("/auth/session")).toHaveLength(0);
  });

  it("signs in with a code by email, asks for the organization, then shows its runs", async () => {
    const user = userEvent.setup();
    const world = withWorkos();
    const { callsTo } = renderApp("/runs", { world });
    expect(await screen.findByRole("heading", { name: "Sign in to Rewind" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /continue with github/i })).toBeInTheDocument();

    await sendCode(user, "ada@example.net");
    const [sent] = callsTo("/auth/magic-auth", "POST");
    expect(sent?.headers.get("Content-Type")).toMatch(/application\/json/);
    expect(JSON.parse((await sent?.body) ?? "")).toEqual({ email: "ada@example.net" });

    // Ada belongs to two organizations: she picks one.
    await user.type(screen.getByLabelText("Code"), codeFor(world, "ada@example.net"));
    expect(await screen.findByRole("heading", { name: "Choose an organization" })).toBeVisible();
    const orgs = screen.getByRole("list", { name: "Organizations" });
    expect(within(orgs).getByText("Globex")).toBeInTheDocument();
    await user.click(within(orgs).getByText("Acme"));

    expect(await screen.findByRole("heading", { name: "All runs" })).toBeInTheDocument();
    const [chosen] = callsTo("/auth/organization", "POST");
    expect(JSON.parse((await chosen?.body) ?? "")).toEqual({ organizationId: "org_01ACME" });
    // `/v1` gets the access token; `/auth` never does.
    const token = world.access.session?.step === "authenticated" ? world.access.session.token : "";
    await waitFor(() =>
      expect(callsTo("/v1/runs")[0]?.headers.get("Authorization")).toBe(`Bearer ${token}`),
    );
    expect(callsTo("/auth/organization")[0]?.headers.get("Authorization")).toBeNull();
    expect(screen.getByLabelText("Organization: Acme")).toBeInTheDocument();
    expect(screen.getByLabelText("Account: Ada Lovelace")).toBeInTheDocument();
  });

  it("says when the code is wrong, and lets one ask for another", async () => {
    const user = userEvent.setup();
    const world = withWorkos();
    renderApp("/runs", { world });
    await sendCode(user, "grace@example.net");
    const right = codeFor(world, "grace@example.net");
    await user.type(screen.getByLabelText("Code"), right === "000000" ? "111111" : "000000");
    expect(await screen.findByRole("alert")).toHaveTextContent(/wrong or has expired/i);

    await user.click(screen.getByRole("button", { name: "Send a new code" }));
    expect(await screen.findByText("A new code was sent.")).toBeInTheDocument();
    await user.clear(screen.getByLabelText("Code"));
    await user.type(screen.getByLabelText("Code"), codeFor(world, "grace@example.net"));
    // Grace has one organization: no choice to make.
    expect(await screen.findByRole("heading", { name: "All runs" })).toBeInTheDocument();
  });

  it("verifies the email when WorkOS asks for it", async () => {
    const user = userEvent.setup();
    const world = withWorkos();
    const { callsTo } = renderApp("/runs", { world });
    await sendCode(user, "alan@example.net");
    await user.type(screen.getByLabelText("Code"), codeFor(world, "alan@example.net"));
    expect(await screen.findByRole("heading", { name: "Verify your email" })).toBeInTheDocument();

    await user.type(screen.getByLabelText("Verification code"), codeFor(world, "alan@example.net"));
    expect(await screen.findByRole("heading", { name: "All runs" })).toBeInTheDocument();
    expect(callsTo("/auth/email-verification", "POST")).toHaveLength(1);
    // Alan's grant is limited to labels: the statistics, from counters that know no labels, are
    // not for him, and none is asked for.
    expect(screen.queryByRole("link", { name: "Tools" })).toBeNull();
    expect(screen.queryByRole("link", { name: "Scripts" })).toBeNull();
    expect(screen.queryByRole("link", { name: "Overview" })).toBeNull();
    // Alan's calls: those of the previous test may still be in flight.
    const alan = callsTo("/v1/whoami").at(-1)?.headers.get("Authorization");
    expect(alan).toBeTruthy();
    // The runs view mounted its reads: the list of runs, and anything it reads beside it.
    await waitFor(() => expect(callsTo("/v1/runs").length).toBeGreaterThan(0));
    const counted = ["/v1/runs/stats", "/v1/runs/failures", "/v1/scripts", "/v1/tools/stats"];
    expect(
      counted
        .flatMap((path) => callsTo(path))
        .filter((c) => c.headers.get("Authorization") === alan),
    ).toHaveLength(0);
  });

  it("puts an unknown email on the waitlist", async () => {
    const user = userEvent.setup();
    const world = withWorkos();
    renderApp("/runs", { world });
    // No code is sent to someone new: the waitlist at once.
    await user.type(await screen.findByLabelText("Email"), "newcomer@example.org");
    await user.click(screen.getByRole("button", { name: /send me a code/i }));
    expect(await screen.findByRole("heading", { name: "Sign-ups are closed" })).toBeVisible();
    expect(world.access.codes.size).toBe(0);
    expect(screen.getByLabelText("Email")).toHaveValue("newcomer@example.org");

    await user.click(screen.getByRole("button", { name: "Join the waitlist" }));
    expect(await screen.findByRole("heading", { name: "You're on the list" })).toBeVisible();
    expect(world.access.waitlist).toEqual(["newcomer@example.org"]);

    await user.click(screen.getByRole("button", { name: /back to sign in/i }));
    expect(await screen.findByRole("heading", { name: "Sign in to Rewind" })).toBeVisible();
  });

  it("lets someone new join the waitlist from the first page", async () => {
    const user = userEvent.setup();
    const world = withWorkos();
    renderApp("/runs", { world });
    await user.type(await screen.findByLabelText("Email"), "early@example.org");
    await user.click(screen.getByRole("button", { name: "Join the waitlist" }));
    expect(await screen.findByRole("heading", { name: "Join the waitlist" })).toBeVisible();
    expect(screen.getByLabelText("Email")).toHaveValue("early@example.org");

    await user.click(screen.getByRole("button", { name: /back to sign in/i }));
    await user.click(await screen.findByRole("button", { name: "Join the waitlist" }));
    await user.click(await screen.findByRole("button", { name: "Join the waitlist" }));
    expect(await screen.findByRole("heading", { name: "You're on the list" })).toBeVisible();
    expect(world.access.waitlist).toEqual(["early@example.org"]);
    expect(world.access.codes.size).toBe(0);
  });

  it("says why a sign-in with GitHub failed, then takes the reason out of the address", async () => {
    window.history.replaceState(null, "", "/?auth_error=state_mismatch");
    renderApp("/", { world: withWorkos() });
    expect(await screen.findByText(/did not start on this page, or took too long/i)).toBeVisible();
    expect(window.location.search).toBe("");
  });

  it("leaves for GitHub with the invitation the page was opened with", async () => {
    const user = userEvent.setup();
    window.history.replaceState(null, "", "/?invitation_token=tok_123");
    const assign = vi.fn();
    renderApp("/", { world: withWorkos(), assign });
    expect(await screen.findByText(/you were invited/i)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /continue with github/i }));
    expect(assign).toHaveBeenCalledWith(
      `${window.location.origin}/auth/github?invitationToken=tok_123`,
    );
  });

  it("keeps the invitation through a sign-in by email, then drops it from the address", async () => {
    const user = userEvent.setup();
    const world = withWorkos();
    const invitation = world.access.invitations.find((i) => i.email === "katherine@example.net");
    window.history.replaceState(null, "", `/?invitation_token=${invitation?.token}`);
    const { callsTo } = renderApp("/", { world });
    await sendCode(user, "katherine@example.net");
    await user.type(screen.getByLabelText("Code"), codeFor(world, "katherine@example.net"));

    // The invitation gave her the role reader in Acme.
    expect(await screen.findByRole("heading", { name: "Overview" })).toBeInTheDocument();
    const [verify] = callsTo("/auth/magic-auth/verify", "POST");
    expect(JSON.parse((await verify?.body) ?? "")).toMatchObject({
      invitationToken: invitation?.token,
    });
    expect(window.location.search).toBe("");
    expect(world.access.grants.get("org_01ACME")?.get("user:user_01KATHERINE")?.role).toBe(
      "reader",
    );
  });

  it("asks for the session at load, as after GitHub's callback", async () => {
    window.history.replaceState(null, "", "/?invitation_token=used");
    const world = signedIn("grace@example.net");
    const { callsTo } = renderApp("/", { world });
    expect(await screen.findByRole("heading", { name: "Overview" })).toBeInTheDocument();
    const [session] = callsTo("/auth/session", "POST");
    expect(session?.headers.get("Content-Type")).toMatch(/application\/json/);
    expect(JSON.parse((await session?.body) ?? "")).toEqual({});
    expect(window.location.search).toBe("");
  });

  it("chooses the organization after GitHub, when the user has several", async () => {
    const user = userEvent.setup();
    const world = withWorkos();
    // The callback kept the step in the cookie.
    world.access.session = { step: "organization_selection_required", userId: "user_01ADA" };
    renderApp("/", { world });
    await user.click(await screen.findByRole("button", { name: /globex/i }));
    // Ada only reads in Globex, which has no run.
    expect(await screen.findByRole("heading", { name: "Overview" })).toBeInTheDocument();
    expect(screen.getByLabelText("Organization: Globex")).toBeInTheDocument();
  });
});
