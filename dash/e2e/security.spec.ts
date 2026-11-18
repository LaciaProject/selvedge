import { expect, test } from "@playwright/test";

import { env } from "./helpers";

// RFC 0003 §4.4: one way in per listener, requests that act must come from the
// dashboard's own pages, and the pages carry a strict policy.

const dash = (path: string) => `${env.dash}${path}`;

test("pages carry the security headers", async () => {
  const r = await fetch(dash("/"));
  expect(r.status).toBe(200);
  const csp = r.headers.get("content-security-policy") ?? "";
  expect(csp).toContain("script-src 'self'");
  expect(csp).toContain("frame-ancestors 'none'");
  expect(r.headers.get("x-frame-options")).toBe("DENY");
  expect(r.headers.get("x-content-type-options")).toBe("nosniff");
  // The app's routes are the app; missing files are missing.
  expect((await fetch(dash("/accounts/x/workers"))).headers.get("content-type")).toContain("text/html");
  expect((await fetch(dash("/assets/missing.js"))).status).toBe(404);
  expect((await fetch(dash("/internal/v1/routes"))).status).toBe(404);
});

test("the dashboard refuses Bearer tokens, and the API session cookies", async ({ page }) => {
  const bearer = await fetch(dash("/client/v4/accounts"), {
    headers: { authorization: `Bearer ${env.adminToken}`, "x-selvedge-dash": "1" },
  });
  expect(bearer.status).toBe(401);
  // Log in, then take the session cookie to the public listener.
  await page.goto("/login");
  const login = await page.request.post("/auth/token", {
    headers: { "x-selvedge-dash": "1", origin: env.dash },
    data: { token: env.token },
  });
  expect(login.status()).toBe(200);
  const cookie = (await page.context().cookies()).find((c) => c.name.includes("selvedge-session"));
  expect(cookie).toBeDefined();
  expect(cookie!.httpOnly).toBe(true);
  expect(cookie!.sameSite).toBe("Lax");
  const api = await fetch(`${env.api}/accounts`, { headers: { cookie: `${cookie!.name}=${cookie!.value}` } });
  expect(api.status).toBe(401);
});

test("requests from other sites are refused", async ({ page }) => {
  await page.goto("/login");
  await page.request.post("/auth/token", {
    headers: { "x-selvedge-dash": "1", origin: env.dash },
    data: { token: env.token },
  });
  const path = `/client/v4/accounts/${env.account}/storage/kv/namespaces`;
  // Without the dashboard's header: what a cross-site <img> or form sends.
  expect((await page.request.get(path)).status()).toBe(403);
  expect((await page.request.get(path, { headers: { "x-selvedge-dash": "1" } })).status()).toBe(200);
  // A change from another origin.
  const evil = await page.request.post(path, {
    headers: { "x-selvedge-dash": "1", origin: "https://evil.example.com" },
    data: { title: "evil" },
  });
  expect(evil.status()).toBe(403);
  // A token login from another origin.
  const forged = await fetch(dash("/auth/token"), {
    method: "POST",
    headers: { "x-selvedge-dash": "1", origin: "https://evil.example.com", "content-type": "application/json" },
    body: JSON.stringify({ token: env.token }),
  });
  expect(forged.status).toBe(403);
});

test("the dashboard cannot be framed", async ({ page }) => {
  await page.setContent(`<iframe src="${env.dash}/login" id="f"></iframe>`);
  const frame = page.frameLocator("#f");
  await expect(frame.locator("body")).not.toContainText("Selvedge", { timeout: 3000 });
});
