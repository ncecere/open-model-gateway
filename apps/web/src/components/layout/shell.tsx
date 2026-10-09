import { useEffect, useState, type ReactNode } from "react";
import { Boxes, LayoutDashboard, Shield, LogOut, UserRound, RefreshCw, House } from "lucide-react";
import type { Session, Workspace } from "../../lib/api";
import { authorityLabel } from "../../lib/access";
import { canAdminister, type DashboardSearch, type Page } from "../../lib/permissions";
import { navigation, navParents, hiddenPages, activeAdminGroup, adminGroups, contextOptions, isAdminPage, personalWorkspace, rememberedPortal, rememberedWorkspace, rememberPortal, rememberWorkspace, scopeSearch, workspaceSections } from "../../lib/navigation";
import { ResourceLink } from "../navigation-link";
import { AppShell, Sidebar, SidebarHeader, SidebarContent, SidebarFooter, SidebarNav, SidebarSection, SidebarItem, SidebarModeSwitch, SidebarUser, Brand, WorkspaceSwitcher, TopBar, Main, useAppShell } from "../ui/app-shell/app-shell";
import { Avatar } from "../ui/avatar/avatar";
import { Badge } from "../ui/badge/badge";
import { MenuGroup, MenuHeader, MenuItem, MenuLinkItem, MenuSeparator } from "../ui/menu/menu";
import { Breadcrumbs, type BreadcrumbItem } from "../ui/breadcrumbs/breadcrumbs";
import { TooltipProvider } from "../ui/tooltip/tooltip";
import { useMediaQuery } from "../../lib/bitop-utils";
import { JumpSearch } from "../jump-search";
import { NotificationBell } from "../notification-bell";
import { ThemeMenuGroup } from "./theme";
import { documentTitle, fitCrumbs, useCurrentCrumbTail, useCurrentResourceName, useCurrentResourceParent, type CrumbParent } from "./breadcrumbs";
import { useAdminGroups } from "./admin-groups";
import s from "./layout.module.css";
import { platformRoleLabels } from "../../lib/people";
export const COMPACT_SHELL_QUERY = "(max-width: 68.75rem)";
function savedCollapsed(): boolean | null { try { const saved = localStorage.getItem("omg.enterprise.sidebarCollapsed"); return saved === null ? null : saved === "1"; } catch { return null; } }
function portal(session: Session, mode: "workspace" | "admin") { try { return rememberedPortal(sessionStorage, session, mode); } catch { return undefined; } }
function context(session: Session) { try { return rememberedWorkspace(sessionStorage, session); } catch { return undefined; } }
const detailParents = navParents;
/**
 * The workspace the sidebar's lower group shows: the page's workspace, else the one last worked
 * in this tab, else Personal. User-level pages (Home, profile) keep the current selection.
 */
export function selectedWorkspace(session: Session, workspace?: Workspace, remembered?: Workspace) { return workspace ?? remembered ?? personalWorkspace(session); }
/**
 * Breadcrumb trail (Grounded: the root crumb carries an icon). Workspace portal: Home, or the selected
 * workspace › page. Admin: Admin › section › [parent record ›] record, e.g. Admin › Models › GPT-6 Luna › OpenAI route.
 */
export function shellCrumbs(session: Session, search: DashboardSearch, label: string, workspace?: Workspace, record?: CrumbParent): (Omit<BreadcrumbItem, "render"> & { to?: DashboardSearch })[] {
  const page = search.page ?? "overview", admin = canAdminister(session) && isAdminPage(page), parent = detailParents[page];
  const between = record ? [{ label: record.label, to: record.to }] : [];
  // Admin › Settings › <page>: settings pages sit under one Settings crumb (its first page).
  const settings = navigation.find(n => n.page === page)?.group === "Settings" ? [{ label: "Settings", to: { page: "settings-general" } as DashboardSearch }] : [];
  if (admin) return [{ label: "Admin", icon: <Shield aria-hidden />, to: { page: "platform-overview" } }, ...settings, ...(parent ? [{ label: page === "workspace-detail" && search.kind === "project" ? "Projects" : navigation.find(n => n.page === parent)!.label, to: { page: page === "workspace-detail" && search.kind === "project" ? "platform-projects" : parent } as DashboardSearch }] : []), ...between, { label }];
  // Workspace records (a request, a key) sit under their list page: Product › Requests › Request 1a2b….
  if (workspace) return [{ label: workspace.name, icon: workspace.kind === "personal" ? <UserRound aria-hidden /> : <Boxes aria-hidden />, to: { page: "overview", ws: workspace.id } }, ...(parent ? [{ label: navigation.find(n => n.page === parent)?.label ?? "", to: { page: parent, ws: workspace.id } as DashboardSearch }] : []), ...between, { label }];
  return [{ label, icon: page === "home" ? <House aria-hidden /> : page === "profile" ? <UserRound aria-hidden /> : undefined }];
}
export function DashboardShell({ session, search, workspace, navigate, refresh, logout, children }: { session: Session; search: DashboardSearch; workspace?: Workspace; navigate: (search: DashboardSearch) => void; refresh: () => void; logout: () => void; children: ReactNode }) {
  const page = search.page ?? "overview", admin = canAdminister(session) && isAdminPage(page), readonly = session.capabilities.platform_read && !session.capabilities.platform_write;
  const [choice, setChoice] = useState(savedCollapsed), compact = useMediaQuery(COMPACT_SHELL_QUERY), narrow = useMediaQuery("(max-width: 37.5rem)");
  const active = selectedWorkspace(session, workspace, context(session));
  const tail = useCurrentCrumbTail(), resourceName = useCurrentResourceName(), resourceParent = useCurrentResourceParent();
  const label = (page.endsWith("detail") ? resourceName : undefined) ?? navigation.find(n => n.page === page)?.label ?? hiddenPages[page] ?? ({ "workspace-detail": search.kind === "project" ? "Project" : "Team", "model-detail": "Model", "provider-detail": "Connection", "catalog-detail": "Catalog", "user-detail": "User", profile: "Your profile", "accept-invitation": "Accept invitation" } as Partial<Record<Page, string>>)[page] ?? "Overview";
  const crumb = (name: string, to: DashboardSearch, icon?: ReactNode): BreadcrumbItem => ({ label: name, icon, render: <ResourceLink search={to} /> });
  const crumbs: BreadcrumbItem[] = shellCrumbs(session, search, label, admin ? undefined : workspace, page.endsWith("detail") ? resourceParent : undefined).map(({ to, ...item }) => to ? crumb(String(item.label), to, item.icon) : item);
  if (tail) { const last = crumbs[crumbs.length - 1]; if (last) crumbs[crumbs.length - 1] = crumb(String(last.label), { ...search, tab: undefined }, last.icon); crumbs.push({ label: tail }); }
  useEffect(() => { document.title = documentTitle(crumbs, session.installation.name); }, [session.installation.name, page, workspace?.name, tail, resourceName, resourceParent?.label]);
  useEffect(() => { try { rememberPortal(sessionStorage, session, search); rememberWorkspace(sessionStorage, session, search); } catch { /* Storage unavailable. */ } }, [session, JSON.stringify(search)]);
  const groups = useAdminGroups(activeAdminGroup(page));
  // Footer identity (Grounded): display name over email when the identity provider sends one; the role is a label.
  const display = session.user.display_name?.trim(), roleLabel = platformRoleLabels[session.user.platform_role] ?? "No platform role";
  const navParent = page === "workspace-detail" && search.kind === "project" ? "platform-projects" : detailParents[page] ?? page;
  const switcher = <WorkspaceSwitcher name={active?.name ?? session.user.email} description={active ? authorityLabel(active) : "No workspace yet"}>
    {contextOptions(session).map(group => <MenuGroup key={group.label} label={group.label}>{group.items.map(item => <MenuLinkItem key={item.ws} render={<ResourceLink search={{ page: "overview", ws: item.ws }} />} icon={<Avatar name={item.label} shape="square" size="xs" decorative />}><span className={s.menuTeam}><span className={s.menuTeamName}>{item.label}</span><span className={s.menuTeamRole}>{authorityLabel(item.workspace)}</span></span></MenuLinkItem>)}</MenuGroup>)}
    <MenuSeparator />
    <MenuLinkItem render={<ResourceLink search={{ page: "home" }} />} icon={<House aria-hidden />}>Home</MenuLinkItem>
  </WorkspaceSwitcher>;
  const nav = admin
    ? adminGroups(page).map(group => <SidebarSection key={group} label={group || undefined} collapsible={!!group} open={group ? groups.isOpen(group) : undefined} onOpenChange={group ? open => groups.setOpen(group, open) : undefined}>{navigation.filter(n => n.group === group).map(item => <SidebarItem key={item.page} label={item.label} icon={<item.icon aria-hidden />} current={navParent === item.page} render={<ResourceLink search={{ page: item.page }} />} />)}</SidebarSection>)
    : workspaceSections(session, active).map(section => <SidebarSection key={section.id} label={section.label}>{section.items.map(item => <SidebarItem key={item.page} label={item.label} icon={<item.icon aria-hidden />} current={navParent === item.page} render={<ResourceLink search={scopeSearch(item.page, active?.id)} />} />)}</SidebarSection>);
  const sidebar = <Sidebar label={admin ? "Admin sidebar" : "Workspace sidebar"}><SidebarHeader><Brand name={session.installation.name} render={<ResourceLink search={{ page: "home" }} />} />{canAdminister(session) && <SidebarModeSwitch label="Portal" items={[{ label: "Workspace", icon: <LayoutDashboard aria-hidden />, current: !admin, render: <ResourceLink search={{ page: "home" }} /> }, { label: "Admin", icon: <Shield aria-hidden />, current: admin, render: <ResourceLink search={portal(session, "admin") ?? { page: "platform-overview" }} /> }]} />}{admin ? <AdminHeader readonly={readonly} /> : switcher}</SidebarHeader>
    <SidebarContent><SidebarNav aria-label="Main">{nav}</SidebarNav></SidebarContent>
    <SidebarFooter><SidebarUser name={display || session.user.email} email={display ? session.user.email : roleLabel}><MenuHeader><strong>{display || session.user.email}</strong>{display ? `${session.user.email} · ${roleLabel}` : roleLabel}</MenuHeader><MenuSeparator /><MenuLinkItem icon={<UserRound aria-hidden />} render={<ResourceLink search={{ page: "profile" }} />}>Your profile</MenuLinkItem><MenuItem icon={<RefreshCw aria-hidden />} onClick={refresh}>Reload my access</MenuItem><MenuSeparator /><ThemeMenuGroup /><MenuSeparator /><MenuItem icon={<LogOut aria-hidden />} onClick={logout}>Sign out</MenuItem></SidebarUser></SidebarFooter></Sidebar>;
  return <TooltipProvider><AppShell collapsed={choice ?? compact} onCollapsedChange={collapsed => { setChoice(collapsed); try { localStorage.setItem("omg.enterprise.sidebarCollapsed", collapsed ? "1" : "0"); } catch { /* Unavailable storage. */ } }} sidebar={sidebar} topbar={<TopBar start={<Breadcrumbs items={fitCrumbs(crumbs, narrow)} className={s.crumbs} />} end={<>{admin && readonly && <Badge tone="warning" dot className={s.adminBadge} title="Read-only"><span className={s.adminBadgeText}>Read-only</span></Badge>}<JumpSearch session={session} workspace={admin ? undefined : active} navigate={navigate} refresh={refresh} /><NotificationBell /></>} />}><Main>{children}</Main></AppShell></TooltipProvider>;
}
function AdminHeader({ readonly }: { readonly: boolean }) { const shell = useAppShell(); return <div className={shell?.collapsed ? s.visuallyHidden : s.adminHeader}><span aria-hidden className={s.adminHeaderIcon}><Shield /></span><span className={s.adminHeaderText}><span className={s.adminHeaderName}>{readonly ? "Platform auditor" : "Platform admin"}</span><span className={s.adminHeaderDescription}>{readonly ? "Configuration and totals, read-only" : "Installation administration"}</span></span></div>; }
