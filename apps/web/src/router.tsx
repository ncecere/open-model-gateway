import { Link, Outlet, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { Home } from "./pages/home";
import { dashboardSearch } from "./lib/permissions";

export function DashboardNotFound() {
  return <main className="mx-auto max-w-3xl px-6 py-20"><h1 className="text-3xl font-semibold">Page not found</h1><p className="mt-4 text-slate-600">That dashboard page does not exist.</p><Link to="/" className="mt-6 inline-block font-medium text-indigo-700 underline">Return to overview</Link></main>;
}
const rootRoute = createRootRoute({ component: Outlet, notFoundComponent: DashboardNotFound });
// Explicit routes deliberately exclude wildcard fallback: unknown asset/API and
// dashboard paths must not turn into a successful overview screen.
function route<const T extends string>(path: T) { return createRoute({ getParentRoute: () => rootRoute, path, validateSearch: dashboardSearch, component: Home }); }
const routes = [
  route("/"), route("/admin"), route("/admin/organizations"), route("/admin/organizations/$org"), route("/admin/teams"), route("/admin/projects"), route("/admin/users"), route("/admin/models"), route("/admin/models/$record"), route("/admin/providers"), route("/admin/providers/$record"), route("/admin/deployments"), route("/admin/deployments/$record"), route("/admin/routing"), route("/admin/pricing"), route("/admin/model-access"), route("/admin/audit"),
  route("/organizations/$org/settings"), route("/organizations/$org/workspaces"), route("/organizations/$org/workspaces/$ws"), route("/organizations/$org/workspaces/$ws/keys"), route("/organizations/$org/workspaces/$ws/models"), route("/organizations/$org/workspaces/$ws/costs"), route("/organizations/$org/workspaces/$ws/settings"), route("/profile"), route("/invitations/accept"),
];
export const dashboardRouteTree = rootRoute.addChildren(routes);
export const router = createRouter({ routeTree: dashboardRouteTree });
declare module "@tanstack/react-router" { interface Register { router: typeof router } }
