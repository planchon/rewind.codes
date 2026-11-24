/** Invite someone to the beta, unless they are already a customer, once the user agrees. */
// Harness actions are called like any action. A failure the handler throws on purpose arrives as an ActionError; an
// answer the handler defers may take days, and the run waits for it durably.
import { crm, ask_user } from "rewind:actions";
import { ActionError } from "rewind";

export default async function main(input: { email: string }) {
  try {
    const existing = await crm.find_customer({ email: input.email });
    return { invited: false, why: `already a customer since ${existing.since}` };
  } catch (error) {
    if (!(error instanceof ActionError) || (error.data as { code?: string } | undefined)?.code !== "not_found") throw error;
  }

  const { answer } = await ask_user({ question: `Invite ${input.email} to the beta?` });
  if (answer !== "yes") return { invited: false, why: "the user said no" };

  const invitation = await crm.invite({ email: input.email });
  return { invited: true, invitation: invitation.id };
}
