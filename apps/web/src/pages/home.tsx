import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { LayoutDashboard, LogOut, Mail, RefreshCw, Shield, UserRound } from "lucide-react";
import { API, ApiError, api, orgPath, type Session, type Organization, type Workspace, type Member } from "../lib/api";
import { canAdminister, canAdministerOrganization, canView, permissions, type Page } from "../lib/permissions";
import { adminGroups, adminDestination, contextOptions, navigation as nav, pageScope, scopeSearch } from "../lib/navigation";
import { ActionProvider, ApiScopeProvider, Button, Empty, ErrorNotice, Heading, useAction, useChoices } from "../components/ui";
import { AppShell, Sidebar, SidebarHeader, SidebarContent, SidebarFooter, SidebarNav, SidebarSection, SidebarItem, SidebarModeSwitch, SidebarUser, Brand as ShellBrand, WorkspaceSwitcher, TopBar, Main } from "../components/ui/app-shell/app-shell";
import { MenuGroup, MenuItem, MenuLinkItem, MenuHeader } from "../components/ui/menu/menu";
import { Breadcrumbs } from "../components/ui/breadcrumbs/breadcrumbs";
import { TooltipProvider } from "../components/ui/tooltip/tooltip";
import { JumpSearch } from "../components/jump-search";
import { Overview, Profile } from "./overview";
import { Keys, ServiceAccounts, WorkspaceMembers, Grants } from "./workspace";
import { AcceptInvitation, AuditHistory, Invitations, OrganizationMembers } from "./organization";
import { Models, Providers, Deployments } from "./catalog";
import { AssignedModels, PlatformModelAccess } from "./model-access";
import { Governance, Costs, Routing, Pricing } from "./governance";
import { Organizations, Teams, PlatformTeams, PlatformUsers, type NavigateScope } from "./hierarchy";

export function Home() {
  const client = useQueryClient();
  const [signedOut, setSignedOut] = useState(false);
  const session = useQuery({ queryKey: ["session"], queryFn: ({ signal }) => api<Session>(`${API}/me`, { signal }), retry: false, enabled: !signedOut, staleTime: 15_000 });
  useEffect(() => {
    const expired = () => { setSignedOut(true); client.clear(); };
    window.addEventListener("omg:unauthorized", expired);
    return () => window.removeEventListener("omg:unauthorized", expired);
  }, [client]);
  if (signedOut || (session.isError && session.error instanceof ApiError && session.error.status === 401)) return <SignIn />;
  if (session.isPending) return <main className="auth-page"><div role="status" className="auth-card"><Brand /><h1>Loading your workspace…</h1></div></main>;
  if (session.isError) return <main className="auth-page"><div className="auth-card"><Brand /><h1>Could not load your session</h1><ErrorNotice error={session.error} retry={() => void session.refetch()} /></div></main>;
  return <ApiScopeProvider session={session.data}><Dashboard session={session.data} onLogout={() => { setSignedOut(true); client.clear(); }} /></ApiScopeProvider>;
}
function Brand() { return <div className="brand"><span className="brand-mark" aria-hidden="true">omg</span><span>Open Model Gateway</span></div>; }
function SignIn() {
  const config = useQuery({ queryKey: ["auth-config"], queryFn: ({ signal }) => api<{ enabled: boolean }>(`${API}/auth/config`, { signal }), retry: false });
  return <main className="auth-page"><section className="auth-card"><Brand /><span className="eyebrow">Workspace dashboard</span><h1>Your models.<br />One workspace.</h1><p>Manage inference access, connect providers, and inspect activity with your organization’s identity.</p>{config.isPending ? <p role="status">Checking sign-in configuration…</p> : config.isError ? <ErrorNotice error={config.error} retry={() => void config.refetch()} /> : config.data.enabled ? <a className="button sign-in" href={`${API}/auth/login`}>Sign in with your organization</a> : <div className="notice"><strong>Sign-in is not configured</strong><p>Ask your gateway operator to enable OIDC authentication. There is no local or API-key dashboard login.</p></div>}<p className="help">An inference API key cannot be used to access this dashboard. Invitation recipients must sign in before accepting a token.</p></section></main>;
}
function Dashboard({ session, onLogout }: { session: Session; onLogout: () => void }) {
  const search = useSearch({ from: "/" });
  const [collapsed, setCollapsed] = useState(() => globalThis.matchMedia?.("(max-width: 48rem)").matches ?? false);
  useEffect(() => {
    const media = window.matchMedia("(max-width: 48rem)");
    const collapseOnNarrow = () => { if (media.matches) setCollapsed(true); };
    media.addEventListener("change", collapseOnNarrow);
    return () => media.removeEventListener("change", collapseOnNarrow);
  }, []);
  const page = search.page ?? (session.user.platform_admin ? "organizations" : "overview");
  const selectedOrganization = pageScope(page) === "platform" ? undefined : search.org ? session.organizations.find((org) => org.id === search.org) : session.organizations[0];
  const members = useChoices<Member>(`${orgPath(selectedOrganization?.id ?? "unselected")}/members`, selectedOrganization?.role === "operator");
  const organization = selectedOrganization ? { ...selectedOrganization, membership_role: members.data?.find((member) => member.user_id === session.user.id && !member.disabled_at)?.role ?? null } : undefined;
  const workspaces = session.workspaces.filter((ws) => ws.organization_id === organization?.id);
  const workspace = search.ws ? workspaces.find((ws) => ws.id === search.ws) : workspaces.find((ws) => ws.kind === "personal") ?? workspaces[0];
  // Search navigation remounts the shell with its dialogs. Restore focus from
  // this stable parent after that remount, without weakening scope isolation.
  const searchNavigationFocus = useRef(false);
  useEffect(() => {
    if (!searchNavigationFocus.current) return;
    searchNavigationFocus.current = false;
    const frame = requestAnimationFrame(() => {
      const heading = document.querySelector<HTMLElement>("#main h1");
      if (heading) { heading.tabIndex = -1; heading.focus({ preventScroll: true }); }
    });
    return () => cancelAnimationFrame(frame);
  }, [page, organization?.id, workspace?.id]);
  // Context changes destroy dialogs and transient secrets, never reusing them across scopes.
  return <ActionProvider key={`${session.user.id}:${organization?.id}:${workspace?.id}:${page}`}><DashboardShell onSearchNavigation={() => { searchNavigationFocus.current = true; }} collapsed={collapsed} onCollapsedChange={setCollapsed} session={session} organization={organization} workspace={workspace} workspaces={workspaces} page={page} invalidScope={pageScope(page) !== "platform" && !!((search.org && !organization) || (pageScope(page) === "workspace" && search.ws && !workspace))} membershipError={members.error} onLogout={onLogout} /></ActionProvider>;
}
function DashboardShell({ session, organization, workspace, workspaces, page, invalidScope, membershipError, onLogout, collapsed, onCollapsedChange, onSearchNavigation }: { onSearchNavigation: () => void; session: Session; organization?: Organization; workspace?: Workspace; workspaces: Workspace[]; page: Page; invalidScope: boolean; membershipError?: unknown; onLogout: () => void; collapsed: boolean; onCollapsedChange: (value: boolean) => void }) {
  const navigate = useNavigate({ from: "/" });
  const client = useQueryClient();
  const ask = useAction();
  const p = permissions(session, organization, workspace);
  const go: NavigateScope = (org, ws, next = "overview") => void navigate({ to: "/", search: scopeSearch(next, org, ws) });
  const scoped = (next: Page) => scopeSearch(next, organization?.id, workspace?.id);
  const admin = nav.some((link) => link.page === page && link.group !== "Workspace");
  const refreshAccess = () => { void client.invalidateQueries({ queryKey: ["session"] }); void client.invalidateQueries({ queryKey: ["api"] }); };
  const createPersonal = () => organization && ask({ title: "Open your personal workspace", description: "Creates or retrieves your private workspace in this organization. Other members cannot access it.", submitLabel: "Open personal workspace", run: async () => { const result = await api<{ id: string }>(`${orgPath(organization.id)}/personal-workspace`, { method: "POST", body: {} }); await client.invalidateQueries({ queryKey: ["session"] }); go(organization.id, result.id); return result; } });
  const choices = contextOptions(session, pageScope(page) === "platform" ? undefined : organization, admin);
  const itemLabel = (item: typeof nav[number]) => item.page === "members" ? workspace?.kind === "project" ? "Project members" : "Team members" : item.label;
  const currentLabel = nav.find((item) => item.page === page) ? itemLabel(nav.find((item) => item.page === page)!) : (page === "profile" ? "Your profile" : "Accept invitation");
  const sidebar = <Sidebar>
    <SidebarHeader>
      <ShellBrand name="Open Model Gateway" render={<Link to="/" search={{}} />} />
      {canAdminister(session) && <SidebarModeSwitch label="Portal" items={[
        { label: "Workspace", icon: <LayoutDashboard aria-hidden />, current: !admin, render: <Link to="/" search={scoped("overview")} /> },
        { label: "Admin", icon: <Shield aria-hidden />, current: admin, render: <Link to="/" search={adminDestination(session, organization, workspace)} /> },
      ]} />}
      {choices.length > 0 && <WorkspaceSwitcher name={admin && pageScope(page) === "platform" ? "Platform" : admin && pageScope(page) === "organization" ? organization?.name ?? "Select organization" : workspace?.name ?? organization?.name ?? "Select workspace"} description={admin ? "Switch context" : workspace?.kind === "personal" ? "Personal · private" : workspace ? `${workspace.kind === "project" ? "Project" : "Team"} · ${workspace.role}` : undefined}>
        {choices.map(group => <MenuGroup key={group.label} label={group.label}>{group.items.map(item => <MenuItem key={item.ws ?? item.org ?? item.page} onClick={() => {
          if (item.page) { go(item.org, item.ws, item.page); return; }
          go(item.org, item.ws, "overview");
        }}>{item.label}</MenuItem>)}</MenuGroup>)}
      </WorkspaceSwitcher>}
    </SidebarHeader>
    <SidebarContent><SidebarNav aria-label="Dashboard">{(admin ? adminGroups(page, session.user.platform_admin, p.organizationAdmin) : ["Workspace"]).map(group => {
      const items = nav.filter(item => item.group === group && canView(item.page, session, organization, workspace));
      if (!items.length) return null;
      return <SidebarSection key={group} label={group}>{items.map(item => <SidebarItem key={item.page} label={itemLabel(item)} icon={<item.icon aria-hidden />} current={page === item.page} render={<Link to="/" search={scoped(item.page)} activeOptions={{ exact: true, includeSearch: true }} />} />)}</SidebarSection>;
    })}</SidebarNav></SidebarContent>
    <SidebarFooter><SidebarUser name={session.user.email} email={session.user.platform_admin ? "Platform admin" : p.organizationAdmin ? "Organization admin" : canAdministerOrganization(session, organization) ? workspace?.kind === "project" && p.workspaceAdmin ? "Project admin" : workspace?.kind === "team" && p.workspaceAdmin ? "Team admin" : "Shared workspace admin" : "Member"}>
      <MenuHeader>{session.user.email}</MenuHeader>
      <MenuLinkItem icon={<UserRound aria-hidden />} render={<Link to="/" search={scoped("profile")} />}>Your profile</MenuLinkItem>
      <MenuLinkItem icon={<Mail aria-hidden />} render={<Link to="/" search={scoped("accept-invitation")} />}>Accept invitation</MenuLinkItem>
      <MenuItem icon={<RefreshCw aria-hidden />} onClick={refreshAccess}>Refresh access</MenuItem>
      <MenuItem icon={<LogOut aria-hidden />} onClick={() => ask({ title: "Sign out?", description: "Your browser session will end. Inference keys are not revoked when you sign out.", submitLabel: "Sign out", run: () => api(`${API}/auth/logout`, { method: "POST", body: {} }), after: onLogout })}>Sign out</MenuItem>
    </SidebarUser></SidebarFooter>
  </Sidebar>;
  const crumbs = [{ label: "Platform" }, ...(pageScope(page) !== "platform" && organization ? [{ label: organization.name }] : []), ...(pageScope(page) === "workspace" && workspace ? [{ label: workspace.name }] : []), { label: currentLabel }];
  return <TooltipProvider><AppShell collapsed={collapsed} onCollapsedChange={onCollapsedChange} sidebar={sidebar} topbar={<TopBar className="gateway-topbar" start={<Breadcrumbs className="gateway-crumbs" items={crumbs} />} end={<JumpSearch session={session} organization={pageScope(page) === "platform" ? undefined : organization} workspace={pageScope(page) === "platform" ? undefined : workspace} navigate={search => { onSearchNavigation(); void navigate({ to: "/", search }); }} refresh={refreshAccess} />} />}>
    <Main>{membershipError != null && <ErrorNotice error={membershipError} retry={() => void client.invalidateQueries({ queryKey: ["api"] })} />}
      {invalidScope ? <><Heading title="Scope not found" /><Empty title="This scope is not available">It may have been removed, or your access has changed. Select an available organization, team or project.</Empty><Button variant="secondary" onClick={() => go()}>Return to your workspace</Button></>
      : pageScope(page) === "platform" ? canView(page, session) ? <PlatformScreen page={page} session={session} go={go} /> : <><Heading title="Access not available" /><p className="notice">Your current role does not allow this screen.</p></>
      : page === "profile" ? <Profile session={session} />
      : page === "accept-invitation" ? <AcceptInvitation email={session.user.email} />
      : !organization ? <><Heading title="Welcome to Open Model Gateway" /><Empty title="No organization access yet">Accept an invitation from an organization administrator{p.createOrganization ? ", or create your first organization." : " to begin."}</Empty><div className="actions"><Button onClick={() => go(undefined, undefined, "accept-invitation")}>Accept invitation</Button>{p.createOrganization && <Button variant="secondary" onClick={() => go(undefined, undefined, "organizations")}>Manage organizations</Button>}</div></>
      : !workspace && pageScope(page) === "workspace" && page !== "governance" ? <><Heading title="Choose a workspace" /><Empty title="No accessible workspace">Choose an existing workspace from the switcher, or manage shared teams and projects from Admin.</Empty>{p.createPersonalWorkspace && <Button onClick={createPersonal}>Open personal workspace</Button>}</>
      : !canView(page, session, organization, workspace) ? <><Heading title="Access not available" /><p className="notice">Your current role does not allow this screen. Personal workspaces cannot be shared or have service accounts.</p></>
      : <>{page === "overview" && p.createPersonalWorkspace && !workspaces.some(ws => ws.kind === "personal") && <div className="actions"><Button variant="secondary" onClick={createPersonal}>Open personal workspace</Button></div>}<Screen page={page} session={session} organization={organization} workspace={workspace} go={go} /></>}
    </Main>
  </AppShell></TooltipProvider>;
}
function PlatformScreen({ page, session, go }: { page: Page; session: Session; go: NavigateScope }) {
  if (page === "organizations") return <Organizations session={session} go={go} />;
  if (page === "platform-teams" || page === "platform-projects") return <PlatformTeams key={page} session={session} go={go} kind={page === "platform-projects" ? "project" : "team"} />;
  if (page === "models") return <Models session={session} />;
  if (page === "providers") return <Providers session={session} />;
  if (page === "deployments") return <Deployments session={session} />;
  if (page === "routing") return <Routing session={session} />;
  if (page === "pricing") return <Pricing session={session} />;
  if (page === "model-access") return <PlatformModelAccess session={session} />;
  if (page === "platform-audit") return <AuditHistory />;
  return <PlatformUsers go={go} />;
}
function Screen({ page, session, organization, workspace, go }: { page: Page; session: Session; organization: Organization; workspace?: Workspace; go: NavigateScope }) {
  const org = { session, organization };
  if (page === "teams" || page === "projects") return <Teams key={page} {...org} go={go} kind={page === "projects" ? "project" : "team"} />;
  if (page === "assigned-models") return <AssignedModels organization={organization} />;
  if (page === "organization-policy") return <Governance {...org} />;
  if (page === "governance") return <Governance {...org} workspace={workspace} />;
  if (page === "organization-members") return <OrganizationMembers organization={organization} onInvite={() => go(organization.id, undefined, "invitations")} />;
  if (page === "invitations") return <Invitations {...org} />;
  if (page === "audit") return <AuditHistory organization={organization} />;
  if (!workspace) return null;
  const scope = { ...org, workspace };
  if (page === "costs") return <Costs {...scope} />;
  if (page === "keys") return <Keys {...scope} />;
  if (page === "service-accounts") return <ServiceAccounts {...scope} />;
  if (page === "members") return <WorkspaceMembers {...scope} />;
  if (page === "grants") return <Grants {...scope} />;
  return <Overview {...scope} />;
}
