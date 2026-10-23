import { rmSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startStack, stopStack } from "./stack.mjs";

export default async function globalSetup() {
  // Fresh personas, fixture ids and axe findings for every run.
  rmSync(path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../target/browser-tests/state"), { recursive: true, force: true });
  const state = await startStack();
  // Operator one-shot commands in acceptance tests use this same disposable DB.
  // Never emit a test-only evaluation route or expose the URL to the SPA.
  const databaseUrl = new URL(state.adminUrl); databaseUrl.pathname = `/${state.database}`;
  process.env.OMG_BROWSER_DATABASE_URL = databaseUrl.href;
  return async () => { await stopStack(state); };
}
