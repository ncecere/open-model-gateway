import { Link, Outlet, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { Home } from "./pages/home";
import { dashboardSearch } from "./lib/permissions";

const rootRoute = createRootRoute({
  component: Outlet,
  notFoundComponent: () => (
    <main className="mx-auto max-w-3xl px-6 py-20">
      <h1 className="text-3xl font-semibold">Page not found</h1>
      <p className="mt-4 text-slate-600">That dashboard page does not exist.</p>
      <Link to="/" className="mt-6 inline-block font-medium text-indigo-700 underline">Return to overview</Link>
    </main>
  ),
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  validateSearch: dashboardSearch,
  component: Home,
});

export const router = createRouter({ routeTree: rootRoute.addChildren([indexRoute]) });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
