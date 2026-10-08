import type { Session, Workspace } from "./api";
import { authorityLabel } from "./access";
import { SELECTED, navigation, portalWorkspaces, scopeSearch } from "./navigation";
import { canView, inWorkspacePortal, type DashboardSearch } from "./permissions";
// Old names still find their new homes (Deployments and Routing live on model pages).
const aliases: Partial<Record<string, string[]>> = { "settings-general": ["settings", "installation", "display name", "logo", "support link", "key lifetime"], policies: ["limits", "defaults", "installation ceiling", "budgets"], "settings-privacy": ["privacy", "data collection", "openrouter", "retention"], "settings-email": ["email", "smtp", "mail", "invitations"], "settings-sign-in": ["sso", "oidc", "single sign-on", "login"], providers: ["provider connections", "providers", "endpoints"], models: ["deployments", "routes", "routing", "aliases", "pricing", "prices"], catalogs: ["availability", "defaults"], home: ["start", "dashboard", "welcome"], "workspace-settings": ["workspace settings", "members", "limits", "audit"], requests: ["executions", "activity", "logs", "requests", "generations", "sessions"], "platform-logs": ["logs", "requests", "generations", "sessions", "executions"] };
export type JumpTarget = { id: string; label: string; group: string; hint?: string; keywords: string[]; search: DashboardSearch };
// Search only /me inventory. Platform cost dimensions never hydrate private workspace pages.
export function jumpTargets(session: Session, workspace?: Workspace): JumpTarget[] {
  const found = session.workspaces.find(item => item.id === workspace?.id), ws = found && inWorkspacePortal(session, found) ? found : undefined;
  const targets: JumpTarget[] = navigation.filter(item => canView(item.page, session, ws) && (item.group !== SELECTED || ws)).map(item => ({ id: `page:${item.page}`, label: item.page === "platform-overview" ? "Admin overview" : item.label, group: "Pages", hint: item.group === SELECTED ? ws!.name : item.group, keywords: [item.page, item.group === SELECTED ? ws!.name : item.group, ...(aliases[item.page] ?? [])], search: scopeSearch(item.page, ws?.id) }));
  // Create forms are Admin-only actions; Auditors (platform_read only) never get mutation shortcuts.
  if (session.capabilities.platform_write) targets.push({ id: "page:model-new", label: "Add model", group: "Pages", hint: "Models", keywords: ["create", "new model", "route", "deployment", "connection"], search: { page: "model-new" } });
  // Pricing lives in the Models table now: "Unpriced models" opens it filtered (Admin › Pricing redirects there too).
  if (session.capabilities.platform_read) targets.push({ id: "page:unpriced-models", label: "Unpriced models", group: "Pages", hint: "Models", keywords: ["pricing", "prices", "unpriced", "cost unknown"], search: { page: "models", layout: "table", pricing: "unpriced" } });
  // Invitations arrive by link; "Accept invitation" is not a destination you search for.
  targets.push({ id: "page:profile", label: "Your profile", group: "Pages", keywords: ["account", "roles"], search: { page: "profile" } });
  for (const w of portalWorkspaces(session)) targets.push({ id: `workspace:${w.id}`, label: w.name, group: w.kind === "personal" ? "Personal" : w.kind === "team" ? "Teams" : "Projects", hint: authorityLabel(w), keywords: [w.kind, w.role ?? ""], search: { page: "overview", ws: w.id } });
  return targets;
}
