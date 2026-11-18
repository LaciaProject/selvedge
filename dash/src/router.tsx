import {
  createRootRoute,
  createRoute,
  createRouter,
  lazyRouteComponent,
  Outlet,
  redirect,
} from "@tanstack/react-router";

import { NotFound } from "./components/NotFound";
import { Shell } from "./components/Shell";
import { ApiError } from "./lib/api";
import { meQuery, queryClient } from "./lib/session";

const str = (v: unknown) => (typeof v === "string" && v !== "" ? v : undefined);
const tabSearch = (s: Record<string, unknown>) => ({ tab: str(s.tab) });

const root = createRootRoute({ component: Outlet, notFoundComponent: NotFound });

const login = createRoute({
  getParentRoute: () => root,
  path: "/login",
  validateSearch: (s: Record<string, unknown>) => ({ error: str(s.error), return_to: str(s.return_to) }),
  component: lazyRouteComponent(() => import("./pages/Login"), "Login"),
});

/** Everything behind a session. */
const app = createRoute({
  getParentRoute: () => root,
  id: "app",
  beforeLoad: async ({ location }) => {
    try {
      return { me: await queryClient.ensureQueryData(meQuery) };
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) {
        throw redirect({ to: "/login", search: { return_to: location.href, error: undefined } });
      }
      throw e;
    }
  },
  component: Shell,
});

const home = createRoute({
  getParentRoute: () => app,
  path: "/",
  component: lazyRouteComponent(() => import("./pages/Home"), "Home"),
});

const account = createRoute({ getParentRoute: () => app, path: "/accounts/$account" });

// Pages load when first visited, so the first screen stays small.
const accountRoutes = [
  createRoute({
    getParentRoute: () => account,
    path: "/",
    component: lazyRouteComponent(() => import("./pages/Overview"), "Overview"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/workers",
    component: lazyRouteComponent(() => import("./pages/workers/Workers"), "Workers"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/workers/$script",
    validateSearch: tabSearch,
    component: lazyRouteComponent(() => import("./pages/workers/Worker"), "Worker"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/kv",
    component: lazyRouteComponent(() => import("./pages/kv/Namespaces"), "Namespaces"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/kv/$namespace",
    component: lazyRouteComponent(() => import("./pages/kv/Namespace"), "Namespace"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/d1",
    component: lazyRouteComponent(() => import("./pages/d1/Databases"), "Databases"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/d1/$database",
    validateSearch: tabSearch,
    component: lazyRouteComponent(() => import("./pages/d1/Database"), "Database"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/r2",
    component: lazyRouteComponent(() => import("./pages/r2/Buckets"), "Buckets"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/r2/$bucket",
    validateSearch: (s: Record<string, unknown>) => ({ ...tabSearch(s), prefix: str(s.prefix) }),
    component: lazyRouteComponent(() => import("./pages/r2/Bucket"), "Bucket"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/queues",
    component: lazyRouteComponent(() => import("./pages/queues/Queues"), "Queues"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/queues/$queue",
    component: lazyRouteComponent(() => import("./pages/queues/Queue"), "Queue"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/workflows",
    component: lazyRouteComponent(() => import("./pages/workflows/Workflows"), "Workflows"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/workflows/$workflow",
    component: lazyRouteComponent(() => import("./pages/workflows/Workflow"), "Workflow"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/containers",
    component: lazyRouteComponent(() => import("./pages/Containers"), "Containers"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/durable-objects",
    component: lazyRouteComponent(() => import("./pages/DurableObjects"), "DurableObjects"),
  }),
  createRoute({
    getParentRoute: () => account,
    path: "/settings",
    validateSearch: tabSearch,
    component: lazyRouteComponent(() => import("./pages/settings/Settings"), "Settings"),
  }),
];

const admin = createRoute({
  getParentRoute: () => app,
  path: "/admin",
  validateSearch: tabSearch,
  component: lazyRouteComponent(() => import("./pages/admin/Admin"), "Admin"),
});

const tree = root.addChildren([login, app.addChildren([home, account.addChildren(accountRoutes), admin])]);

export const router = createRouter({
  routeTree: tree,
  defaultPreload: "intent",
  scrollRestoration: true,
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
