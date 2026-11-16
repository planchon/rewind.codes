// The sign-in pages, with WorkOS: GitHub, or a 6-digit code sent by email; then whatever step
// the server answers (an organization to choose, an email to verify, the waitlist). Someone new
// may also open the waitlist from the first page.
import { useMutation } from "@tanstack/react-query";
import { ArrowLeft, Building2, Check, LogOut, Mail } from "lucide-react";
import { type FormEvent, type ReactNode, useEffect, useRef, useState } from "react";
import { ApiError, type AuthAnswer } from "../api/client";
import type { AuthConfig, Organization } from "../api/types";
import { ErrorNote } from "../components/ErrorNote";
import { Brand, GithubMark } from "./Brand";
import { useSession } from "./session";

export function SignIn() {
  const { phase } = useSession();
  if (phase.kind === "signed_out")
    return <SignInStart config={phase.config} notice={phase.notice} />;
  if (phase.kind === "waitlisted") return <OnTheList email={phase.email} />;
  if (phase.kind !== "step") return null;
  const state = phase.state;
  switch (state.status) {
    case "organization_selection_required":
      return <ChooseOrganization email={state.user.email} organizations={state.organizations} />;
    case "email_verification_required":
      return <VerifyEmail email={state.email} />;
    case "sign_up_closed":
      return <Waitlist email={state.email ?? ""} refused footer={<UseAnotherAccount />} />;
  }
}

function Card({ children, label }: { children: ReactNode; label: string }) {
  return (
    <div className="gate">
      <section className="gate-card" aria-label={label}>
        <Brand />
        {children}
      </section>
    </div>
  );
}

/** A wrong or expired code is the usual refusal: say so plainly. */
function CodeError({ error }: { error: unknown }) {
  if (error instanceof ApiError && error.slug === "invalid-request") {
    return <ErrorNote error={error} title="This code is wrong or has expired" />;
  }
  return <ErrorNote error={error} />;
}

/** Six digits, as they are typed or pasted; sent as soon as the sixth is there. */
function CodeField({
  onComplete,
  pending,
  label = "Code",
}: {
  onComplete: (code: string) => void;
  pending: boolean;
  label?: string;
}) {
  const [code, setCode] = useState("");
  const sent = useRef("");
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (code.length === 6 && !pending) {
      sent.current = code;
      onComplete(code);
    }
  };
  return (
    <form className="stack" onSubmit={submit}>
      <label className="field col">
        <span className="field-label">{label}</span>
        <input
          className="input code-input"
          inputMode="numeric"
          autoComplete="one-time-code"
          pattern="[0-9]{6}"
          maxLength={6}
          placeholder="000000"
          spellCheck={false}
          value={code}
          onChange={(e) => {
            const digits = e.target.value.replace(/\D/g, "").slice(0, 6);
            setCode(digits);
            if (digits.length === 6 && digits !== sent.current && !pending) {
              sent.current = digits;
              onComplete(digits);
            }
          }}
          // biome-ignore lint/a11y/noAutofocus: the only field of the step
          autoFocus
        />
      </label>
      <button type="submit" className="btn primary block" disabled={code.length !== 6 || pending}>
        {pending ? "Checking…" : "Continue"}
      </button>
    </form>
  );
}

function SignInStart({ config, notice }: { config: AuthConfig; notice: string | null }) {
  const { api, invitationToken, signInWithGithub, advance } = useSession();
  const [email, setEmail] = useState("");
  const [sentTo, setSentTo] = useState<string | null>(null);
  const [resent, setResent] = useState(false);
  const [joining, setJoining] = useState(false);
  const github = config.methods.includes("github");
  const magic = config.methods.includes("magic_auth");

  const send = useMutation({
    mutationFn: (to: string) => api.sendMagicCode(to, invitationToken),
    // No code for someone new while sign-ups are closed: the waitlist instead.
    onSuccess: (answer, to) => (answer ? advance(answer) : setSentTo(to)),
  });
  const verify = useMutation({
    mutationFn: ({ to, code }: { to: string; code: string }) =>
      api.verifyMagicCode(to, code, invitationToken),
    onSuccess: (answer: AuthAnswer) => advance(answer),
  });

  if (sentTo !== null) {
    return (
      <Card label="Enter the code">
        <h1>Check your email</h1>
        <p>
          A 6-digit code is on its way to <strong>{sentTo}</strong>. It expires after a few minutes.
        </p>
        <CodeField
          pending={verify.isPending}
          onComplete={(code) => verify.mutate({ to: sentTo, code })}
        />
        {verify.error && <CodeError error={verify.error} />}
        {send.error && <ErrorNote error={send.error} />}
        <div className="gate-actions">
          <button
            type="button"
            className="link-btn"
            onClick={() => {
              verify.reset();
              send.reset();
              setResent(false);
              setSentTo(null);
            }}
          >
            <ArrowLeft size={12} /> Use another email
          </button>
          <button
            type="button"
            className="link-btn"
            disabled={send.isPending}
            onClick={() => {
              verify.reset();
              setResent(true);
              send.mutate(sentTo);
            }}
          >
            {send.isPending ? "Sending…" : "Send a new code"}
          </button>
        </div>
        {resent && send.isSuccess && (
          <p className="gate-foot" role="status">
            A new code was sent.
          </p>
        )}
      </Card>
    );
  }

  if (joining) {
    return (
      <Waitlist
        email={email.trim()}
        refused={false}
        footer={
          <div className="gate-actions">
            <button type="button" className="link-btn" onClick={() => setJoining(false)}>
              <ArrowLeft size={12} /> Back to sign in
            </button>
          </div>
        }
      />
    );
  }

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const to = email.trim();
    if (to) send.mutate(to);
  };

  return (
    <Card label="Sign in">
      <h1>Sign in to Rewind</h1>
      {notice && (
        <div className="gate-note" role="status">
          {notice}
        </div>
      )}
      {invitationToken && (
        <div className="gate-note" role="status">
          You were invited to Rewind: sign in to accept the invitation.
        </div>
      )}
      {github && (
        <button type="button" className="btn block" onClick={signInWithGithub}>
          <GithubMark /> Continue with GitHub
        </button>
      )}
      {github && magic && <div className="gate-or">or</div>}
      {magic && (
        <form className="stack" onSubmit={submit}>
          <label className="field col">
            <span className="field-label">Email</span>
            <input
              className="input"
              type="email"
              autoComplete="email"
              placeholder="you@company.com"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              required
            />
          </label>
          <button
            type="submit"
            className="btn primary block"
            disabled={!email.trim() || send.isPending}
          >
            <Mail size={13} /> {send.isPending ? "Sending…" : "Send me a code"}
          </button>
        </form>
      )}
      {send.error && <ErrorNote error={send.error} />}
      {config.waitlist && !invitationToken && (
        <p className="gate-foot">
          New to Rewind? Sign-ups go through a waitlist, and approved people get an invitation by
          email.{" "}
          <button type="button" className="link-btn" onClick={() => setJoining(true)}>
            Join the waitlist
          </button>
        </p>
      )}
    </Card>
  );
}

function UseAnotherAccount() {
  const { signOut } = useSession();
  const [error, setError] = useState<unknown>(null);
  return (
    <>
      <div className="gate-actions">
        <button
          type="button"
          className="link-btn"
          onClick={() => signOut().catch((e: unknown) => setError(e))}
        >
          <LogOut size={12} /> Use another account
        </button>
      </div>
      {error ? <ErrorNote error={error} /> : null}
    </>
  );
}

function ChooseOrganization({
  email,
  organizations,
}: {
  email: string;
  organizations: Organization[];
}) {
  const { api, advance } = useSession();
  const choose = useMutation({
    mutationFn: (id: string) => api.selectOrganization(id),
    onSuccess: (answer) => advance(answer),
  });
  return (
    <Card label="Choose an organization">
      <h1>Choose an organization</h1>
      <p>
        <strong>{email}</strong> belongs to several organizations. You can switch between them
        later.
      </p>
      <ul className="choices" aria-label="Organizations">
        {organizations.map((o) => (
          <li key={o.id}>
            <button
              type="button"
              className="choice"
              disabled={choose.isPending}
              onClick={() => choose.mutate(o.id)}
            >
              <Building2 size={15} aria-hidden="true" />
              <span className="choice-name">{o.name}</span>
              <span className="mono muted choice-id">{o.id}</span>
            </button>
          </li>
        ))}
      </ul>
      {choose.error && <ErrorNote error={choose.error} />}
      <UseAnotherAccount />
    </Card>
  );
}

function VerifyEmail({ email }: { email: string }) {
  const { api, advance } = useSession();
  const verify = useMutation({
    mutationFn: (code: string) => api.verifyEmail(code),
    onSuccess: (answer) => advance(answer),
  });
  return (
    <Card label="Verify your email">
      <h1>Verify your email</h1>
      <p>
        Before the first sign-in, the email has to be verified: a 6-digit code was sent to{" "}
        <strong>{email}</strong>.
      </p>
      <CodeField
        label="Verification code"
        pending={verify.isPending}
        onComplete={(code) => verify.mutate(code)}
      />
      {verify.error && <CodeError error={verify.error} />}
      <UseAnotherAccount />
    </Card>
  );
}

/**
 * The waitlist's form: after a sign-in refused because sign-ups are closed (`refused`), or opened
 * from the first page by someone new.
 */
function Waitlist({
  email: given,
  refused,
  footer,
}: {
  email: string;
  refused: boolean;
  footer: ReactNode;
}) {
  const { api, waitlisted } = useSession();
  const [email, setEmail] = useState(given);
  const join = useMutation({
    mutationFn: (to: string) => api.joinWaitlist(to),
    onSuccess: (_, to) => waitlisted(to),
  });
  const submit = (e: FormEvent) => {
    e.preventDefault();
    const to = email.trim();
    if (to) join.mutate(to);
  };
  return (
    <Card label="Join the waitlist">
      <h1>{refused ? "Sign-ups are closed" : "Join the waitlist"}</h1>
      <p>
        {refused &&
          (given ? (
            <>
              <strong>{given}</strong> has no account here, and no invitation.{" "}
            </>
          ) : (
            "This account is not known here, and has no invitation. "
          ))}
        {refused
          ? "Join the waitlist: once the operator approves you, an invitation arrives by email."
          : "Rewind opens to new people through a waitlist. Once the operator approves you, an invitation arrives by email."}
      </p>
      <form className="stack" onSubmit={submit}>
        <label className="field col">
          <span className="field-label">Email</span>
          <input
            className="input"
            type="email"
            autoComplete="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            required
          />
        </label>
        <button
          type="submit"
          className="btn primary block"
          disabled={!email.trim() || join.isPending}
        >
          {join.isPending ? "Joining…" : "Join the waitlist"}
        </button>
      </form>
      {join.error && <ErrorNote error={join.error} />}
      {footer}
    </Card>
  );
}

function OnTheList({ email }: { email: string }) {
  const { backToSignIn } = useSession();
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => heading.current?.focus(), []);
  return (
    <Card label="On the waitlist">
      <div className="gate-done" aria-hidden="true">
        <Check size={18} />
      </div>
      <h1 ref={heading} tabIndex={-1}>
        You're on the list
      </h1>
      <p>
        When the operator approves <strong>{email}</strong>, an invitation arrives there by email.
        Then sign in here with that email, by a code or with GitHub. If it already has an account,
        sign in now.
      </p>
      <div className="gate-actions">
        <button type="button" className="link-btn" onClick={backToSignIn}>
          <ArrowLeft size={12} /> Back to sign in
        </button>
      </div>
    </Card>
  );
}
