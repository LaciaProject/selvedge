import { request } from "node:http";

import { expect, type Page, type TestInfo } from "@playwright/test";

import en from "../src/i18n/en.json" with { type: "json" };
import zh from "../src/i18n/zh-CN.json" with { type: "json" };

/** Where test/e2e/dash.sh started things. */
export const env = {
  dash: process.env.DASH_URL ?? "http://localhost:8790",
  api: process.env.API_URL ?? "http://localhost:8787/client/v4",
  adminToken: process.env.ADMIN_TOKEN ?? "",
  account: process.env.ACCOUNT ?? "",
  token: process.env.TOKEN ?? "",
  gateway: process.env.GATEWAY ?? "http://127.0.0.1:8080",
  subdomain: process.env.SUBDOMAIN ?? "",
};

export const workerHost = () => `dash-app.${env.subdomain}.workers.test`;

/** A request to the Worker through the gateway. (fetch() cannot set Host.) */
export function workerGet(path: string): Promise<number> {
  const gw = new URL(env.gateway);
  return new Promise((resolve, reject) => {
    const req = request({ host: gw.hostname, port: gw.port, path, headers: { host: workerHost() } }, (res) => {
      res.resume();
      res.on("end", () => resolve(res.statusCode ?? 0));
    });
    req.on("error", reject);
    req.end();
  });
}

type Messages = Record<string, unknown>;

/**
 * A message in the test's language, from the same files the dashboard uses:
 * the tests check the words a reader sees, in each language.
 */
export function tr(info: TestInfo, key: string, values: Record<string, string | number> = {}): string {
  const messages = (info.project.name === "zh-CN" ? zh : en) as Messages;
  const lookup = (k: string) => k.split(".").reduce<unknown>((o, p) => (o as Messages | undefined)?.[p], messages);
  const v = lookup(key) ?? lookup(`${key}_other`);
  if (typeof v !== "string") throw new Error(`no message ${key}`);
  // The tests match what is on screen: values filled in, markup gone.
  return v.replace(/\{\{(\w+)\}\}/g, (_, k) => String(values[k] ?? "")).replace(/<\/?\w+>/g, "");
}

/** Calls the public API with a Bearer token. */
export async function apiCall(path: string, init: { method?: string; token?: string; body?: unknown } = {}) {
  const res = await fetch(`${env.api}${path}`, {
    method: init.method ?? "GET",
    headers: {
      authorization: `Bearer ${init.token ?? env.adminToken}`,
      ...(init.body !== undefined ? { "content-type": "application/json" } : {}),
    },
    body: init.body !== undefined ? JSON.stringify(init.body) : undefined,
  });
  return res;
}

export async function tokenLogin(page: Page, info: TestInfo, token = env.token) {
  await page.goto("/login");
  await page.getByLabel(tr(info, "login.token")).fill(token);
  await page.getByRole("button", { name: tr(info, "login.withToken") }).click();
  await expect(page).toHaveURL(/\/accounts\//);
}

/** Logs in through Dex as one of dex.template.yaml's people. */
export async function oidcLogin(page: Page, email: string) {
  await page.goto("/login");
  await page.getByRole("button", { name: "Log in with Dex" }).click();
  await page.locator("#login").fill(email);
  await page.locator("#password").fill("password");
  await page.locator("#submit-login").click();
  await page.waitForURL((u) => u.toString().startsWith(env.dash) && !u.pathname.startsWith("/auth/"));
}

/** What /auth/me says about the page's session. */
export async function me(page: Page) {
  const r = await page.request.get("/auth/me", { headers: { "x-selvedge-dash": "1" } });
  return { status: r.status(), body: r.ok() ? (await r.json()).result : null };
}
