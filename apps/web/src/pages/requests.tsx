/*
 * Workspace › Logs (formerly Requests): the shared Logs page in this
 * workspace's scope. Tabs, filters, tiles and tables live in ./logs; this
 * module keeps the workspace entry point and the helpers other pages and
 * tests import.
 *
 * Privacy is the server's: members see requests made with their own keys,
 * workspace admins everyone's, personal workspaces only their owner. Metadata
 * only; prompts and responses are never stored or shown.
 */
import type { Scope } from "./workspace";
import { LogsPage } from "./logs";
export { PeriodControl, requestColumnIds, requestDefaultHidden, requestNarrowHidden, requestPageSearch, requestView, requestViewSearch, requestsDescription } from "./logs";

export function Requests({ workspace }: Scope) {
  return <LogsPage scope={{ kind: "workspace", workspace }} />;
}
