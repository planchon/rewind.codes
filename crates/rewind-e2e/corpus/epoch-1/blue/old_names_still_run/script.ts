// Code written before actions were named: it imports "rewind:tools", catches a ToolError and
// reads its `tool`. The old names are declared nowhere, and still run.
import { payments } from "rewind:tools";
import { ToolError, ActionError } from "rewind";

export default async function main() {
  try {
    await payments.charge({ amount: 100 });
    return { charged: true, checked: false };
  } catch (error) {
    if (error instanceof ToolError && error.outcomeUnknown) {
      const status = await payments.status({ amount: 100 });
      return {
        charged: status.charged,
        checked: true,
        tool: error.tool,
        same: ToolError === ActionError && error.tool === error.action,
      };
    }
    throw error;
  }
}
