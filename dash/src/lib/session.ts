import { QueryClient, queryOptions } from "@tanstack/react-query";

import { api } from "./api";
import type { AuthConfig, Me } from "./types";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 15_000, retry: (n, e) => n < 2 && !(e instanceof Error && "status" in e && (e as { status: number }).status < 500) },
  },
});

export const meQuery = queryOptions({
  queryKey: ["me"],
  queryFn: () => api<Me>("/auth/me"),
  retry: false,
  staleTime: 60_000,
});

export const authConfigQuery = queryOptions({
  queryKey: ["auth-config"],
  queryFn: () => api<AuthConfig>("/auth/config"),
  staleTime: Infinity,
});

export async function loginWithToken(token: string) {
  await api("/auth/token", { method: "POST", json: { token } });
  queryClient.clear();
}

/** Ends the session, and the provider's too if it offers that. */
export async function logout() {
  let next = "/login";
  try {
    const r = await api<{ logout_url: string | null }>("/auth/logout", { method: "POST" });
    if (r.logout_url) next = r.logout_url;
  } finally {
    queryClient.clear();
    window.location.assign(next);
  }
}
