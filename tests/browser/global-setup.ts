import { rmSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startStack, stopStack } from "./stack.mjs";

export default async function globalSetup() {
  // Fresh personas, fixture ids and axe findings for every run.
  rmSync(path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../target/browser-tests/state"), { recursive: true, force: true });
  const state = await startStack();
  return async () => { await stopStack(state); };
}
