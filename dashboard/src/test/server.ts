// The MSW server every test runs against: no request leaves the process.
import { setupServer } from "msw/node";

export const server = setupServer();
