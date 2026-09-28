import { useEffect, useRef, useState } from "react";
import { useNavigate, useRouterState } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { LayoutDashboard, LogOut, Mail, RefreshCw, Shield, UserRound } from "lucide-react";
import { API, ApiError, api, orgPath, type Session, type Organization, type Workspace } from "../lib/api";
import { canAdminister, canView, permissions, type Page, type DashboardSearch } from "../lib/permissions";
import { adminGroups, contextOptions, navigation as nav, pageScope, scopeSearch, isAdminPage, resolveLegacyDashboardLocation, resolveDashboardSearch, rememberPortal, rememberedPortal, rememberWorkspace, rememberedWorkspace, sidebarWorkspace, clearRememberedPortals } from "../lib/navigation";
import { dashboardHref } from "../lib/locations";
import { authorityLabel } from "../lib/access";
import { DashboardNavigationProvider, ResourceLink } from "../components/navigation-link";
import { ActionProvider, ApiScopeProvider, Button, Empty, ErrorNotice, Heading, useAction } from "../components/ui";
import { AppShell, Sidebar, SidebarHeader, SidebarContent, SidebarFooter, SidebarNav, SidebarSection, SidebarItem, SidebarModeSwitch, SidebarUser, Brand as ShellBrand, WorkspaceSwitcher, TopBar, Main } from "../components/ui/app-shell/app-shell";
import { MenuGroup, MenuItem, MenuLinkItem, MenuHeader } from "../components/ui/menu/menu";
import { Breadcrumbs } from "../components/ui/breadcrumbs/breadcrumbs";
import { TooltipProvider } from "../components/ui/tooltip/tooltip";
import { JumpSearch } from "../components/jump-search";
import { Overview, Profile } from "./overview";
import { Keys, Grants } from "./workspace";
import { AcceptInvitation, AuditHistory } from "./organization";
import { Models, Providers, Deployments } from "./catalog";
import { PlatformModelAccess } from "./model-access";
import { Costs, Routing, Pricing } from "./governance";
import { Organizations, PlatformTeams, PlatformUsers, type NavigateScope } from "./hierarchy";
import { OrganizationDetail, WorkspaceSettings } from "./resource-details";
import { PlatformOverview } from "./start";
import { ModelDetail, ProviderDetail, DeploymentDetail } from "./catalog-details";

function clearPortalHistory() { try { clearRememberedPortals(window.sessionStorage); } catch { /* Storage may be disabled. */ } }
function lastPortal(session: Session, portal: "admin" | "workspace") { try { return rememberedPortal(window.sessionStorage, session, portal); } catch { return undefined; } }
function lastWorkspace(session: Session, org?: string) { try { return org ? rememberedWorkspace(window.sessionStorage, session, org) : undefined; } catch { return undefined; } }
export function Home() {
  const client = useQueryClient();
  const [signedOut, setSignedOut] = useState(false);
  const session = useQuery({ queryKey: ["session"], queryFn: ({ signal }) => api<Session>(`${API}/me`, { signal }), retry: false, enabled: !signedOut, staleTime: 15_000 });
  useEffect(() => {
    const expired = () => { clearPortalHistory(); setSignedOut(true); client.clear(); };
    window.addEventListener("omg:unauthorized", expired);
    return () => window.removeEventListener("omg:unauthorized", expired);
  }, [client]);
  const unauthorized = session.isError && session.error instanceof ApiError && session.error.status === 401;
  useEffect(() => { if (unauthorized) { clearPortalHistory(); setSignedOut(true); client.clear(); } }, [unauthorized, client]);
  if (signedOut || unauthorized) return <SignIn />;
  if (session.isPending) return <main className="auth-page"><div role="status" className="auth-card"><Brand /><h1>Loading your workspace…</h1></div></main>;
  if (session.isError) return <main className="auth-page"><div className="auth-card"><Brand /><h1>Could not load your session</h1><ErrorNotice error={session.error} retry={() => void session.refetch()} /></div></main>;
  return <ApiScopeProvider session={session.data}><Dashboard session={session.data} onLogout={() => { clearPortalHistory(); setSignedOut(true); client.clear(); }} /></ApiScopeProvider>;
}
function Brand() { return <div className="brand"><span className="brand-mark" aria-hidden="true">omg</span><span>Open Model Gateway</span></div>; }
function SignIn() {
  const config = useQuery({ queryKey: ["auth-config"], queryFn: ({ signal }) => api<{ enabled: boolean }>(`${API}/auth/config`, { signal }), retry: false });
  return <main className="auth-page"><section className="auth-card"><Brand /><span className="eyebrow">Workspace dashboard</span><h1>Your models.<br />One workspace.</h1><p>Manage inference access, connect providers, and inspect activity with your organization’s identity.</p>{config.isPending ? <p role="status">Checking sign-in configuration…</p> : config.isError ? <ErrorNotice error={config.error} retry={() => void config.refetch()} /> : config.data.enabled ? <a className="button sign-in" href={`${API}/auth/login`}>Sign in with your organization</a> : <div className="notice"><strong>Sign-in is not configured</strong><p>Ask your gateway operator to enable OIDC authentication. There is no local or API-key dashboard login.</p></div>}<p className="help">An inference API key cannot be used to access this dashboard. Invitation recipients must sign in before accepting a token.</p></section></main>;
}
function Dashboard({ session, onLogout }: { session: Session; onLogout: () => void }) {
  const href = useRouterState({ select: state => state.location.href });
  const navigate = useNavigate();
  const [collapsed, setCollapsed] = useState(() => globalThis.matchMedia?.("(max-width: 48rem)").matches ?? false);
  useEffect(() => {
    const media = window.matchMedia?.("(max-width: 48rem)");
    const collapseOnNarrow = () => { if (media?.matches) setCollapsed(true); };
    media?.addEventListener("change", collapseOnNarrow);
    return () => media?.removeEventListener("change", collapseOnNarrow);
  }, []);
  const parsed = resolveLegacyDashboardLocation(href, session);
  const start = href === "/" ? lastPortal(session, session.user.platform_admin ? "admin" : "workspace") : undefined;
  const search = resolveDashboardSearch(start ?? parsed ?? {}, session);
  const page = search.page!;
  const organization = session.organizations.find(org => org.id === search.org);
  const workspaces = session.workspaces.filter(ws => ws.organization_id === organization?.id);
  const workspace = workspaces.find(ws => ws.id === search.ws);
  const invalidScope = !!((search.org && !organization) || (search.ws && !workspace));
  const standalone = page === "profile" || page === "accept-invitation";
  const actionScope = `${session.user.id}:${search.org}:${search.ws}:${page}:${search.tab}:${search.record}:${search.scope}:${search.view}:${search.recipientKind}:${search.recipient}`;
  const searchNavigationFocus = useRef(false);
  const tabFocus = useRef<string | undefined>(undefined);
  const previousScope = useRef<string | undefined>(undefined);
  useEffect(() => {
    // Replacement happens only after /me has supplied the authorized context.
    if (!parsed) return;
    const canonical = dashboardHref(search);
    if (canonical !== href) void navigate({ href: canonical, replace: true });
    if (!invalidScope) try { rememberPortal(window.sessionStorage, session, search); rememberWorkspace(window.sessionStorage, session, search); } catch { /* Storage may be disabled. */ }
  }, [href, session, navigate, invalidScope]); // search is derived solely from href and session
  useEffect(() => {
    if (previousScope.current === actionScope && !searchNavigationFocus.current) return;
    const frame = requestAnimationFrame(() => {
      previousScope.current = actionScope;
      const label = searchNavigationFocus.current ? undefined : tabFocus.current;
      searchNavigationFocus.current = false;
      tabFocus.current = undefined;
      const list = label === undefined ? undefined : Array.from(document.querySelectorAll<HTMLElement>('[role="tablist"]')).find(item => item.getAttribute("aria-label") === label);
      const selected = list?.querySelector<HTMLElement>('[role="tab"][aria-selected="true"]');
      if (selected) { selected.focus({ preventScroll: true }); return; }
      const heading = document.querySelector<HTMLElement>("#main h1");
      if (heading) { heading.tabIndex = -1; heading.focus({ preventScroll: true }); }
    });
    return () => cancelAnimationFrame(frame);
  }, [actionScope, href]);
  const navigateSearch = (next: DashboardSearch) => {
    const activeTab = document.activeElement?.closest('[role="tab"]');
    tabFocus.current = activeTab?.closest('[role="tablist"]')?.getAttribute("aria-label") ?? undefined;
    const resolved = resolveDashboardSearch(next, session);
    const destination = dashboardHref(resolved);
    if (searchNavigationFocus.current && destination === href) {
      const heading = document.querySelector<HTMLElement>("#main h1");
      if (heading) { heading.tabIndex = -1; heading.focus({ preventScroll: true }); }
      searchNavigationFocus.current = false;
    }
    void navigate({ href: destination });
  };
  if (!parsed) return <main className="auth-page"><Heading title="Page not found" /><p>That dashboard page does not exist.</p><ResourceLink search={{}}>Return to overview</ResourceLink></main>;
  return <DashboardNavigationProvider search={search} navigate={navigateSearch}><ActionProvider key={actionScope}><DashboardShell search={search} navigate={navigateSearch} onSearchNavigation={() => { searchNavigationFocus.current = true; }} collapsed={collapsed} onCollapsedChange={setCollapsed} session={session} organization={organization} workspace={workspace} workspaces={workspaces} page={page} invalidScope={!standalone && invalidScope} onLogout={onLogout} /></ActionProvider></DashboardNavigationProvider>;
}
function DashboardShell({ session, organization, workspace, workspaces, page, search, navigate, invalidScope, onLogout, collapsed, onCollapsedChange, onSearchNavigation }: { search: DashboardSearch; navigate: (search: DashboardSearch) => void; onSearchNavigation: () => void; session: Session; organization?: Organization; workspace?: Workspace; workspaces: Workspace[]; page: Page; invalidScope: boolean; onLogout: () => void; collapsed: boolean; onCollapsedChange: (value: boolean) => void }) {
  const client = useQueryClient();
  const ask = useAction();
  const p = permissions(session, organization, workspace);
  const go: NavigateScope = (org, ws, next = "overview") => navigate(scopeSearch(next, org, ws));
  const navigationWorkspace = sidebarWorkspace(session, search, lastWorkspace(session, organization?.id));
  const scoped = (next: Page) => scopeSearch(next, organization?.id, navigationWorkspace?.id);
  const admin = isAdminPage(page);
  const refreshAccess = () => { void client.invalidateQueries({ queryKey: ["session"] }); void client.invalidateQueries({ queryKey: ["api"] }); };
  const createPersonal = () => organization && ask({ title: "Open your personal workspace", description: "Creates or retrieves your private workspace in this organization. Other members cannot access it.", submitLabel: "Open personal workspace", run: async () => { const result = await api<{ id: string }>(`${orgPath(organization.id)}/personal-workspace`, { method: "POST", body: {} }); await client.invalidateQueries({ queryKey: ["session"] }); go(organization.id, result.id); return result; } });
  const choices = contextOptions(session, organization);
  const currentLabel = nav.find(item => item.page === page)?.label ?? ({ "organization-detail": "Organization", "model-detail": "Model", "provider-detail": "Provider connection", "deployment-detail": "Deployment", profile: "Your profile", "accept-invitation": "Accept invitation" } as Partial<Record<Page, string>>)[page] ?? "Overview";
  const sidebar = <Sidebar>
    <SidebarHeader>
      <ShellBrand name="Open Model Gateway" render={<ResourceLink search={{}} />} />
      {canAdminister(session) && <SidebarModeSwitch label="Portal" items={[
        { label: "Workspace", icon: <LayoutDashboard aria-hidden />, current: !admin, render: <ResourceLink search={lastPortal(session, "workspace") ?? { page: "overview" }} /> },
        { label: "Admin", icon: <Shield aria-hidden />, current: admin, render: <ResourceLink search={lastPortal(session, "admin") ?? { page: "platform-overview" }} /> },
      ]} />}
      {admin ? <div className="platform-context"><Shield aria-hidden size={16} /> Platform</div> : choices.length > 0 && <WorkspaceSwitcher name={navigationWorkspace?.name ?? organization?.name ?? "Select workspace"} description={navigationWorkspace?.kind === "personal" ? "Personal · private" : navigationWorkspace ? `${navigationWorkspace.kind === "project" ? "Project" : "Team"} · ${authorityLabel(navigationWorkspace)}` : undefined}>
        {choices.map(group => <MenuGroup key={group.label} label={group.label}>{group.items.map(item => <MenuLinkItem key={item.ws ?? item.org ?? item.page} render={<ResourceLink search={scopeSearch(item.page ?? "overview", item.org, item.ws)} />}>{item.label}</MenuLinkItem>)}</MenuGroup>)}
      </WorkspaceSwitcher>}
    </SidebarHeader>
    <SidebarContent><SidebarNav aria-label="Dashboard">{(admin ? adminGroups(page, session.user.platform_admin) : ["Workspace"]).map(group => {
      const items = nav.filter(item => item.group === group && canView(item.page, session, organization, navigationWorkspace));
      if (!items.length) return null;
      return <SidebarSection key={group} label={group}>{items.map(item => <SidebarItem key={item.page} label={item.label} icon={<item.icon aria-hidden />} current={page === item.page || page === "organization-detail" && item.page === "organizations" || page === "model-detail" && item.page === "models" || page === "provider-detail" && item.page === "providers" || page === "deployment-detail" && item.page === "deployments"} render={<ResourceLink search={scoped(item.page)} />} />)}</SidebarSection>;
    })}</SidebarNav></SidebarContent>
    <SidebarFooter><SidebarUser name={session.user.email} email={session.user.platform_admin ? "Platform admin" : p.organizationAdmin ? "Organization admin" : p.workspaceAdmin && workspace?.kind !== "personal" ? "Shared workspace admin" : "Member"}>
      <MenuHeader>{session.user.email}</MenuHeader>
      <MenuLinkItem icon={<UserRound aria-hidden />} render={<ResourceLink search={{ page: "profile" }} />}>Your profile</MenuLinkItem>
      <MenuLinkItem icon={<Mail aria-hidden />} render={<ResourceLink search={{ page: "accept-invitation" }} />}>Accept invitation</MenuLinkItem>
      <MenuItem icon={<RefreshCw aria-hidden />} onClick={refreshAccess}>Refresh access</MenuItem>
      <MenuItem icon={<LogOut aria-hidden />} onClick={() => ask({ title: "Sign out?", description: "Your browser session will end. Inference keys are not revoked when you sign out.", submitLabel: "Sign out", run: () => api(`${API}/auth/logout`, { method: "POST", body: {} }), after: onLogout })}>Sign out</MenuItem>
    </SidebarUser></SidebarFooter>
  </Sidebar>;
  const crumb = (label: string, target: DashboardSearch) => ({ label, render: <ResourceLink search={target} /> });
  const crumbs = [
    crumb(admin ? "Platform" : "Workspace", admin ? { page: "platform-overview" } : { page: "overview" }),
    ...(organization ? [crumb(organization.name, { page: admin ? "organization-detail" : p.organizationAdmin ? "organization-settings" : "overview", org: organization.id })] : []),
    ...(workspace ? [crumb(workspace.name, { page: "overview", org: organization?.id, ws: workspace.id })] : []),
    ...(search.record && ["model-detail", "provider-detail", "deployment-detail"].includes(page) ? [crumb(page === "model-detail" ? "Models" : page === "provider-detail" ? "Provider connections" : "Deployments", { page: page === "model-detail" ? "models" : page === "provider-detail" ? "providers" : "deployments" })] : []),
    { label: currentLabel },
  ];
  const onTabChange = (tab: string) => navigate({ page, org: search.org, ws: search.ws, record: pageScope(page) === "platform" ? search.record : undefined, tab });
  return <TooltipProvider><AppShell collapsed={collapsed} onCollapsedChange={onCollapsedChange} sidebar={sidebar} topbar={<TopBar className="gateway-topbar" start={<Breadcrumbs className="gateway-crumbs" items={crumbs} />} end={<JumpSearch session={session} organization={admin ? undefined : organization} workspace={admin ? undefined : navigationWorkspace} navigate={next => { onSearchNavigation(); navigate(next); }} refresh={refreshAccess} />} />}>
    <Main>{invalidScope ? <><Heading title="Scope not found" /><Empty title="This scope is not available">It may have been removed, or your access has changed. Select an available organization, team or project.</Empty><Button variant="secondary" onClick={() => go()}>Return to your workspace</Button></>
      : page === "profile" ? <Profile session={session} />
      : page === "accept-invitation" ? <AcceptInvitation email={session.user.email} />
      : admin && !canView(page, session, organization, workspace) ? <AccessUnavailable />
      : pageScope(page) === "platform" ? <PlatformScreen page={page} session={session} go={go} search={search} onTabChange={onTabChange} />
      : !organization ? <><Heading title="Welcome to Open Model Gateway" /><Empty title="No organization access yet">Accept an invitation from an organization administrator{p.createOrganization ? ", or create your first organization." : " to begin."}</Empty><div className="actions"><Button onClick={() => go(undefined, undefined, "accept-invitation")}>Accept invitation</Button>{p.createOrganization && <Button variant="secondary" onClick={() => go(undefined, undefined, "organizations")}>Manage organizations</Button>}</div></>
      : !workspace && pageScope(page) === "workspace" ? <><Heading title="Choose a workspace" /><Empty title="No accessible workspace">Choose an existing workspace from the switcher. Organization administrators can create teams and projects in Organization settings.</Empty>{p.createPersonalWorkspace && <Button onClick={createPersonal}>Open personal workspace</Button>}</>
      : !canView(page, session, organization, workspace) ? <AccessUnavailable />
      : <>{page === "overview" && p.createPersonalWorkspace && !workspaces.some(ws => ws.kind === "personal") && <div className="actions"><Button variant="secondary" onClick={createPersonal}>Open personal workspace</Button></div>}<Screen page={page} session={session} organization={organization} workspace={workspace} go={go} search={search} onTabChange={onTabChange} /></>}
    </Main>
  </AppShell></TooltipProvider>;
}
function AccessUnavailable() { return <><Heading title="Access not available" /><p className="notice">Your current role does not allow this screen.</p></>; }
function PlatformScreen({ page, session, go, search, onTabChange }: { page: Page; session: Session; go: NavigateScope; search: DashboardSearch; onTabChange: (tab: string) => void }) {
  if (page === "platform-overview") return <PlatformOverview session={session} />;
  if (page === "organizations") return <Organizations session={session} go={go} />;
  if (page === "platform-teams" || page === "platform-projects") return <PlatformTeams key={page} session={session} go={go} kind={page === "platform-projects" ? "project" : "team"} />;
  const detail = { session, id: search.record ?? "", tab: search.tab, onTabChange };
  if (page === "model-detail") return <ModelDetail {...detail} />;
  if (page === "provider-detail") return <ProviderDetail {...detail} />;
  if (page === "deployment-detail") return <DeploymentDetail {...detail} />;
  if (page === "models") return <Models session={session} />;
  if (page === "providers") return <Providers session={session} />;
  if (page === "deployments") return <Deployments session={session} />;
  if (page === "routing") return <Routing session={session} />;
  if (page === "pricing") return <Pricing session={session} />;
  if (page === "model-access") return <PlatformModelAccess session={session} />;
  if (page === "platform-audit") return <AuditHistory />;
  return <PlatformUsers go={go} />;
}
function Screen({ page, session, organization, workspace, go, search, onTabChange }: { page: Page; session: Session; organization: Organization; workspace?: Workspace; go: NavigateScope; search: DashboardSearch; onTabChange: (tab: string) => void }) {
  if (page === "organization-detail" || page === "organization-settings") return <OrganizationDetail session={session} organization={organization} tab={search.tab} onTabChange={onTabChange} go={go} platform={page === "organization-detail"} />;
  if (!workspace) return null;
  const scope = { session, organization, workspace };
  if (page === "workspace-settings") return <WorkspaceSettings {...scope} tab={search.tab} onTabChange={onTabChange} />;
  if (page === "costs") return <Costs {...scope} />;
  if (page === "keys") return <Keys {...scope} />;
  if (page === "grants") return <Grants {...scope} />;
  return <Overview {...scope} />;
}
