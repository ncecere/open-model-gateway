import { useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { API, api, platformPath, wsPath, type Collection, type Model, type PlatformUser, type Provider, type Session, type Workspace } from "../lib/api";
import type { MeKey } from "../lib/home";
/** Usable keys first: a rotated key's revoked predecessor (same name) comes after it. */
const keyRank = (status: string) => ({ active: 0, disabled: 1, expired: 2, revoked: 3 } as Record<string, number>)[status] ?? 4;
import type { WorkspaceCatalogModel } from "../lib/model-setup";
import type { DirectoryWorkspace } from "../lib/people";
import { inWorkspacePortal, type DashboardSearch } from "../lib/permissions";
import type { RequestPage } from "../lib/requests";
import { jumpTargets } from "../lib/search";
import { CommandPalette, CommandPaletteTrigger, useCommandPaletteShortcut, type Command, type CommandGroup } from "./ui/command-palette/command-palette";
import { useColorMode } from "./ui/color-mode/color-mode";
import { useApiScope } from "./ui";
import { themeCommands } from "./layout/theme";
import s from "./layout/layout.module.css";

/** A request id or the start of one, as lists show it ("3cbfb500"): 8+ hex characters, dashes allowed. */
export const requestIdQuery = (q: string) => /^[0-9a-f][0-9a-f-]{7,35}$/i.test(q.trim());
type Hit = { id: string; label: string; hint?: string; search: DashboardSearch };
export type EntityGroup = { label: string; hits: Hit[] };

/**
 * Entity results for ⌘K from existing APIs, within the caller's own visibility:
 * models (selected workspace's catalog; Admin catalog for platform readers),
 * your own API keys (/me/keys), requests by id prefix in the selected workspace
 * (the server applies request privacy), and for platform readers users,
 * teams/projects and connections. Nothing here widens access: every list is
 * one the caller can already open.
 */
export function useEntityResults(session: Session, workspace: Workspace | undefined, query: string, open: boolean): { groups: EntityGroup[]; loading: boolean } {
  const scope = useApiScope(), q = query.trim(), on = open && q.length >= 2, lower = q.toLowerCase();
  const ws = workspace && inWorkspacePortal(session, workspace) ? workspace : undefined, platform = session.capabilities.platform_read;
  const enc = encodeURIComponent(q);
  const get = <T,>(key: string, path: string, enabled: boolean) => useQuery({ queryKey: ["api", scope, path, "jump", key], enabled, retry: false, staleTime: 30_000, queryFn: ({ signal }) => api<T>(path, { signal }) });
  /* eslint-disable react-hooks/rules-of-hooks -- fixed call order: every source is always declared. */
  const models = get<Collection<WorkspaceCatalogModel>>("models", ws ? `${wsPath(ws.id)}/catalog?q=${enc}&limit=8` : "", on && !!ws);
  const adminModels = get<Collection<Model>>("admin-models", `${platformPath}/models?q=${enc}&limit=8`, on && platform);
  const keys = get<Collection<MeKey>>("keys", `${API}/me/keys?status=all&limit=200`, open);
  const requests = get<RequestPage>("requests", ws ? `${wsPath(ws.id)}/requests?q=${encodeURIComponent(lower)}&limit=5&start_date=${isoDay(-92)}&end_date=${isoDay(1)}` : "", open && !!ws && requestIdQuery(q));
  const users = get<Collection<PlatformUser>>("users", `${platformPath}/users?q=${enc}&limit=5`, on && platform);
  const shared = get<Collection<DirectoryWorkspace>>("workspaces", `${platformPath}/workspaces?q=${enc}&limit=5`, on && platform);
  const connections = get<Collection<Provider>>("connections", `${platformPath}/providers?limit=200`, open && platform);
  /* eslint-enable react-hooks/rules-of-hooks */
  const has = (text: string | null | undefined) => !!text && text.toLowerCase().includes(lower);
  const groups: EntityGroup[] = [];
  if (on && ws && models.data) groups.push({ label: `Models in ${ws.name}`, hits: models.data.data.map(m => ({ id: `model:${m.model_id}`, label: m.display_name || m.public_name, hint: m.public_name, search: { page: "workspace-model", ws: ws.id, record: m.model_id } })) });
  if (on && platform && adminModels.data) groups.push({ label: "Models (Admin)", hits: adminModels.data.data.map(m => ({ id: `admin-model:${m.id}`, label: m.display_name || m.public_name, hint: m.public_name, search: { page: "model-detail", record: m.id } })) });
  if (on && keys.data) groups.push({ label: "Your API keys", hits: keys.data.data.filter(k => has(k.name) || k.id.startsWith(lower)).sort((a, b) => keyRank(a.status) - keyRank(b.status)).slice(0, 8).map(k => ({ id: `key:${k.id}`, label: k.name, hint: `${k.workspace.name} · ${k.status[0]!.toUpperCase()}${k.status.slice(1)}`, search: { page: "key-detail", ws: k.workspace.id, record: k.id } })) });
  if (ws && requestIdQuery(q) && requests.data) groups.push({ label: `Requests in ${ws.name}`, hits: requests.data.data.map(r => ({ id: `request:${r.root_request_id}`, label: `Request ${r.root_request_id.slice(0, 8)}`, hint: `${r.model} · ${r.key.name}`, search: { page: "request-detail", ws: ws.id, record: r.root_request_id } })) });
  if (on && platform && users.data) groups.push({ label: "Users", hits: users.data.data.map(u => ({ id: `user:${u.id}`, label: u.email ?? "User", hint: u.disabled_at ? "Suspended" : undefined, search: { page: "user-detail", record: u.id } })) });
  if (on && platform && shared.data) groups.push({ label: "Teams and projects (Admin)", hits: shared.data.data.filter(w => w.kind !== "personal").map(w => ({ id: `shared:${w.id}`, label: w.name, hint: w.kind === "project" ? "Project" : "Team", search: { page: "workspace-detail", record: w.id, kind: w.kind } })) });
  if (on && platform && connections.data) groups.push({ label: "Connections", hits: connections.data.data.filter(c => has(c.name) || has(c.provider)).slice(0, 5).map(c => ({ id: `connection:${c.id}`, label: c.name, hint: c.enabled ? undefined : "Disabled", search: { page: "provider-detail", record: c.id } })) });
  const loading = [models, adminModels, requests, users, shared].some(x => x.fetchStatus === "fetching");
  return { groups: groups.filter(g => g.hits.length), loading };
}
function isoDay(offset: number) { return new Date(Date.now() + offset * 86_400_000).toISOString().slice(0, 10); }

export function JumpSearch({ session, workspace, navigate, refresh }: { session: Session; workspace?: Workspace; navigate: (search: DashboardSearch) => void; refresh: () => void }) {
  const [open, setOpen] = useState(false), [query, setQuery] = useState(""), [debounced, setDebounced] = useState(""); const navigated = useRef(false); const { mode } = useColorMode();
  useEffect(() => { const t = setTimeout(() => setDebounced(query), 200); return () => clearTimeout(t); }, [query]);
  const changeOpen = (next: boolean) => { if (next) navigated.current = false; else { setQuery(""); setDebounced(""); } setOpen(next); };
  useCommandPaletteShortcut(() => { if (!open && document.querySelector('[role="dialog"], [role="alertdialog"]')) return; changeOpen(!open); });
  const go = (search: DashboardSearch) => () => { navigated.current = true; navigate(search); };
  const groups: CommandGroup[] = [];
  for (const target of jumpTargets(session, workspace)) { let group = groups.find(g => g.label === target.group); if (!group) { group = { label: target.group, items: [] }; groups.push(group); } group.items.push({ id: target.id, label: target.label, hint: target.hint, keywords: target.keywords, onSelect: go(target.search) }); }
  // Server results already match what was typed: keep their order and let them match the query as typed.
  const entities = useEntityResults(session, workspace, debounced, open);
  for (const g of entities.groups) groups.push({ label: g.label, keepOrder: true, items: g.hits.map((h): Command => ({ id: h.id, label: h.label, hint: h.hint, keywords: [debounced, query], onSelect: go(h.search) })) });
  groups.push({ label: "Actions", items: [{ id: "refresh", label: "Reload my access", keywords: ["refresh", "reload", "permissions"], onSelect: refresh }, ...themeCommands(mode)] });
  const typed = query.trim();
  return <><CommandPaletteTrigger label="Search or jump to…" className={s.search} onClick={() => changeOpen(true)} /><CommandPalette open={open} onOpenChange={changeOpen} query={query} onQueryChange={setQuery} finalFocus={() => { if (!navigated.current) return true; const heading = document.querySelector<HTMLElement>("#main h1"); if (!heading) return true; heading.tabIndex = -1; return heading; }} groups={groups} label="Search or jump to" placeholder="Search pages, models, keys, request IDs…" emptyText={entities.loading || typed !== debounced.trim() ? "Searching…" : `No results for “${typed}”`} /></>;
}
