import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useLocation, useNavigate } from "@tanstack/react-router";
import { abortRequests, api, ApiError, API, type Session, type Workspace } from "../lib/api";
import { canView, type DashboardSearch } from "../lib/permissions";
import { dashboardHref, parseDashboardLocation } from "../lib/locations";
import { clearRememberedPortals, landingSearch, rememberPortal, rememberWorkspace, resolveDashboardSearch, isAdminPage } from "../lib/navigation";
import { inWorkspacePortal } from "../lib/permissions";
import { HomePortal } from "./home-portal";
import { Requests } from "./requests";
import { RequestDetailPage } from "./request-detail";
import { KeyDetail } from "./key-detail";
import { WorkspaceModelPage } from "./workspace-model";
import { ActionProvider, ApiScopeProvider, Button, ErrorNotice, Heading, Panel, Alert, Stack } from "../components/ui";
import { DashboardNavigationProvider } from "../components/navigation-link";
import { DashboardShell } from "../components/layout/shell";
import { AlertDialog } from "../components/ui/dialog/dialog";
import { CrumbProvider } from "../components/layout/breadcrumbs";
import { PlatformTeams, PlatformUsers, UserDetail, OidcMappings, CostCenters } from "./hierarchy";
import { PlatformModelAccess, CatalogDetail } from "./model-access";
import { Models, Providers, Deployments } from "./catalog";
import { AddModel } from "./model-setup";
import { ModelDetail, ProviderDetail, DeploymentDetail } from "./catalog-details";
import { WorkspaceDetail, WorkspaceSettings } from "./resource-details";
import { Overview, Profile } from "./overview";
import { AcceptInvitation, AuditHistory } from "./organization";
import { Keys, Grants } from "./workspace";
import { Governance, Costs, PlatformCosts, Routing, Pricing, PlatformPolicies } from "./governance";
import { PlatformOverview } from "./start";
import s from "./shared.module.css";
import signIn from "./sign-in.module.css";
import { Card, CardBody } from "../components/ui/card/card";
import { LogIn } from "lucide-react";
import { toast } from "../components/ui/toast/toast";
/** The sign-in URL: returns to the current dashboard page afterwards (the server validates the path and drops anything unsafe). */
export function loginHref(location: { pathname: string; search: string } | undefined = typeof window === "undefined" ? undefined : window.location): string {
  if (!location) return "/api/v1/auth/login";
  const params = new URLSearchParams(location.search); params.delete("auth_error");
  const here = `${location.pathname}${params.size ? `?${params}` : ""}`;
  return here === "/" || !here.startsWith("/") || here.startsWith("//") ? "/api/v1/auth/login" : `/api/v1/auth/login?return_to=${encodeURIComponent(here)}`;
}
/** Grounded-style centered sign-in (brand, card, one SSO action). Provenance: Grounded web/src/session.tsx SignInPage. */
const SIGNED_OUT = "omg.enterprise.signedOut";
/** Sign-out reloads the page, so the confirmation travels in sessionStorage and is shown once on the sign-in page (review #49). */
export function takeSignedOutNotice(storage: Pick<Storage, "getItem" | "removeItem"> = window.sessionStorage): boolean {
  try { const set = storage.getItem(SIGNED_OUT) === "1"; if (set) storage.removeItem(SIGNED_OUT); return set; } catch { return false; }
}
export function AuthRequired({ error, refresh, denied = false }: { error?: unknown; refresh: () => void; denied?: boolean }) {
  useEffect(() => { if (takeSignedOutNotice()) toast.success("You're signed out"); }, []);
  return <main className={signIn.page}><div className={signIn.column}>
    <div className={signIn.brand}><span aria-hidden className={signIn.mark}><span className={signIn.accent} /></span><h1 className={signIn.title}>Open Model Gateway</h1><p className={signIn.subtitle}>Models for your personal, team and project workspaces</p></div>
    <Card className={signIn.card}><CardBody className={signIn.body}><Stack gap={5}>
      <div><h2 className={signIn.heading}>Sign in</h2><p className={signIn.lead}>Use your organization account to continue.</p></div>
      {denied && <Alert tone="warning" title="No access yet">Your account doesn't have access yet. Ask a platform admin to add you.</Alert>}
      {error !== undefined && !(error instanceof ApiError && error.status === 401) && <ErrorNotice error={error} retry={refresh} />}
      <Button block render={<a href={loginHref()} />}><LogIn aria-hidden /> Sign in with single sign-on</Button>
    </Stack></CardBody></Card>
    <p className={signIn.note}>Signing in alone doesn't give access; a platform admin gives you a role.</p>
  </div></main>;
}
function PermissionNotice({ title = "Access not available", description = "Your live session does not authorize this page. Refresh access or switch to a workspace where you have the required capability." }: { title?: string; description?: string }) { return <Stack gap={6} className={s.page}><Heading title={title} description={description} /><Alert tone="info">Access is based on current /me capabilities, not remembered URLs or inferred workspace membership.</Alert></Stack>; }
export function DashboardContent({ session, search, workspace, navigate }: { session: Session; search: DashboardSearch; workspace?: Workspace; navigate: (next: DashboardSearch) => void }) {
  const changeTab = (tab: string) => navigate({ ...search, tab: tab === "overview" ? undefined : tab, q: undefined, offset: undefined }), props = { session, id: search.record ?? "", tab: search.tab, onTabChange: changeTab };
  if (!canView(search.page ?? "overview", session, workspace)) return <PermissionNotice />;
  switch (search.page) {
    case "platform-overview": return <PlatformOverview session={session} />;
    case "users": return <PlatformUsers session={session} />;
    case "user-detail": return search.record ? <UserDetail {...props} /> : <PermissionNotice title="Missing user identifier" />;
    case "platform-teams": return <PlatformTeams session={session} kind="team" />;
    case "platform-projects": return <PlatformTeams session={session} kind="project" />;
    case "workspace-detail": return search.record ? <WorkspaceDetail {...props} kind={search.kind} /> : <PermissionNotice title="Missing workspace identifier" />;
    case "catalogs": return <PlatformModelAccess session={session} />;
    case "catalog-detail": return search.record ? <CatalogDetail {...props} /> : <PermissionNotice title="Missing catalog identifier" />;
    case "models": return <Models session={session} />;
    case "model-new": return <AddModel session={session} connection={search.connection} />;
    case "providers": return <Providers session={session} />;
    case "deployments": return <Deployments session={session} />;
    case "model-detail": return search.record ? <ModelDetail {...props} /> : <PermissionNotice title="Missing model identifier" />;
    case "provider-detail": return search.record ? <ProviderDetail {...props} /> : <PermissionNotice title="Missing provider identifier" />;
    case "deployment-detail": return search.record ? <DeploymentDetail {...props} /> : <PermissionNotice title="Missing deployment identifier" />;
    case "policies": return <PlatformPolicies session={session} />;
    case "platform-costs": return <PlatformCosts session={session} />;
    case "cost-centers": return <CostCenters session={session} />;
    case "oidc": return <OidcMappings session={session} />;
    case "platform-audit": return <AuditHistory />;
    case "routing": return <Routing session={session} />;
    case "pricing": return <Pricing session={session} />;
    case "home": return <HomePortal session={session} />;
    case "profile": return <Profile session={session} />;
    case "accept-invitation": return <AcceptInvitation email={session.user.email} />;
    default: if (!workspace) return <PermissionNotice title="Select an accessible workspace" />;
  }
  switch (search.page) {
    case "overview": return <Overview session={session} workspace={workspace} />;
    case "keys": return <Keys session={session} workspace={workspace} />;
    case "requests": return <Requests session={session} workspace={workspace} />;
    case "request-detail": return search.record ? <RequestDetailPage session={session} workspace={workspace} id={search.record} /> : <PermissionNotice title="Missing request identifier" />;
    case "key-detail": return search.record ? <KeyDetail session={session} workspace={workspace} id={search.record} /> : <PermissionNotice title="Missing key identifier" />;
    case "grants": return <Grants session={session} workspace={workspace} />;
    case "workspace-model": return search.record ? <WorkspaceModelPage session={session} workspace={workspace} id={search.record} /> : <PermissionNotice title="Missing model identifier" />;
    case "workspace-settings": return <WorkspaceSettings session={session} workspace={workspace} tab={search.tab} onTabChange={changeTab} />;
    case "governance": return <Governance session={session} workspace={workspace} />;
    case "costs": return <Costs session={session} workspace={workspace} />;
    case "invitations": return <AcceptInvitation email={session.user.email} />;
    default: return <PermissionNotice title="Page not found" />;
  }
}
export function Home() {
  const location = useLocation(), routerNavigate = useNavigate(), client = useQueryClient(), [ended, setEnded] = useState(false), [rechecking, setRechecking] = useState(false), [logoutError, setLogoutError] = useState<unknown>(), [loggingOut, setLoggingOut] = useState(false), [confirmLogout, setConfirmLogout] = useState(false), mounted = useRef(true);
  const session = useQuery({ queryKey: ["session"], queryFn: ({ signal }) => api<Session>(`${API}/me`, { signal }), retry: false, refetchOnWindowFocus: true, staleTime: 0, enabled: !ended });
  const authorization = session.data ? JSON.stringify(session.data) : undefined, identity = useRef<string | undefined>(undefined);
  const navigate = (next: DashboardSearch, replace = false) => { const target = new URL(dashboardHref(next), window.location.origin); void routerNavigate({ to: target.pathname as "/", search: Object.fromEntries(target.searchParams), replace }); };
  const query = new URLSearchParams(); for (const [key, value] of Object.entries(location.search)) if (value !== undefined) query.set(key, String(value));
  const parsed = parseDashboardLocation(`${location.pathname}${query.size ? `?${query}` : ""}`);
  const search: DashboardSearch = session.data ? resolveDashboardSearch(parsed ?? { page: "overview" }, session.data) : { page: "overview" };
  const workspace = session.data?.workspaces.find(w => w.id === search.ws);
  useEffect(() => { mounted.current = true; const onUnauthorized = () => { abortRequests(); void client.cancelQueries(); client.clear(); setEnded(true); };
    const refreshAccess = async () => { setRechecking(true); abortRequests(); await client.cancelQueries({ predicate: q => q.queryKey[0] !== "session" }); client.removeQueries({ predicate: q => q.queryKey[0] !== "session" }); await session.refetch(); if (mounted.current) setRechecking(false); };
    window.addEventListener("omg:unauthorized", onUnauthorized); window.addEventListener("omg:access-refresh", refreshAccess);
    return () => { mounted.current = false; window.removeEventListener("omg:unauthorized", onUnauthorized); window.removeEventListener("omg:access-refresh", refreshAccess); };
  }, [client, session.refetch]);
  useEffect(() => { if (session.error instanceof ApiError && [401, 403].includes(session.error.status)) { abortRequests(); void client.cancelQueries(); client.clear(); setEnded(true); if (session.error.status === 403) setLogoutError(session.error); } }, [session.error, client]);
  useEffect(() => { if (identity.current && authorization && identity.current !== authorization) {
    const staleScope = (q: { queryKey: readonly unknown[] }) => q.queryKey[0] === "api" && q.queryKey[1] !== authorization;
    void client.cancelQueries({ predicate: staleScope }); client.removeQueries({ predicate: staleScope });
  } identity.current = authorization; }, [authorization, client]);
  useEffect(() => { if (!session.data) return; const current = session.data;
    // Sign-in (and the bare root) lands on Home with Personal selected. Unavailable locations fall back to Home too.
    let selection: Storage | undefined; try { selection = window.sessionStorage; } catch { /* Storage is optional. */ }
    const restore: DashboardSearch = { page: "home" };
    if (!parsed || location.pathname === "/") { navigate(landingSearch(current, selection), true); return; }
    if (new URL(dashboardHref(search), window.location.origin).pathname !== location.pathname) { navigate(search, true); return; }
    // The Workspace portal follows membership: platform staff manage other teams and projects from Admin.
    if (search.ws && !current.workspaces.some(w => w.id === search.ws && inWorkspacePortal(current, w))) { navigate(restore, true); return; }
    if (isAdminPage(search.page ?? "overview") && !current.capabilities.platform_read) { navigate(restore, true); return; }
    if (search.ws && workspace?.disabled_at) { navigate(restore, true); return; }
    rememberPortal(window.localStorage, current, search); rememberWorkspace(window.localStorage, current, search);
  }, [session.data, location.pathname, location.search]);
  useEffect(() => { const heading = document.querySelector<HTMLElement>("#main h1"); if (heading) { heading.tabIndex = -1; heading.focus({ preventScroll: true }); } }, [location.pathname]);
  const logout = async (confirmed = false) => { if (loggingOut) return; if (!confirmed && document.querySelector('[data-dirty="true"]')) { setConfirmLogout(true); return; } setConfirmLogout(false); setLoggingOut(true); setLogoutError(undefined); const current = session.data;
    abortRequests(); await client.cancelQueries(); client.clear(); setEnded(true); if (current) { clearRememberedPortals(window.localStorage); try { clearRememberedPortals(window.sessionStorage); } catch { /* Storage is optional. */ } }
    try { await api(`${API}/auth/logout`, { method: "POST" }); try { window.sessionStorage.setItem(SIGNED_OUT, "1"); } catch { /* Storage is optional. */ } if (mounted.current) window.location.assign("/"); } catch (error) { if (mounted.current) { setLogoutError(error); setLoggingOut(false); } }
  };
  const callbackDenied = query.get("auth_error") === "access_denied" || new URLSearchParams(window.location.search).get("auth_error") === "access_denied";
  if (ended) return <AuthRequired denied={callbackDenied} error={logoutError} refresh={() => { setEnded(false); void session.refetch(); }} />;
  if (session.isPending || rechecking) return <main className={s.page}><p role="status">Checking current access…</p></main>;
  if (session.isError || !session.data) return <AuthRequired denied={callbackDenied} error={session.error} refresh={() => void session.refetch()} />;
  const current = session.data;
  // The complete live authorization snapshot keys all content and dialogs. Revocation remounts the
  // scope, aborts transport, and drops query entries before another identity can inspect them.
  return <ApiScopeProvider session={current}><DashboardNavigationProvider search={search} navigate={navigate}><CrumbProvider><ActionProvider key={`${authorization}:${location.pathname}`}><DashboardShell session={current} search={search} workspace={workspace} navigate={navigate} logout={() => void logout()} refresh={() => void session.refetch()}><DashboardContent key={`${location.pathname}:${workspace?.id ?? "platform"}`} session={current} search={search} workspace={workspace} navigate={navigate} /></DashboardShell><AlertDialog open={confirmLogout} onOpenChange={setConfirmLogout} title="Sign out without saving?" description="Unsaved changes and one-time credentials will be cleared. In-flight requests will be aborted." confirmLabel="Sign out" cancelLabel="Keep editing" onConfirm={() => void logout(true)} /></ActionProvider></CrumbProvider></DashboardNavigationProvider></ApiScopeProvider>;
}
