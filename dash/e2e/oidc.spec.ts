import { type BrowserContext, expect, type Page, test } from "@playwright/test";

import { apiCall, env, me, oidcLogin } from "./helpers";

// Single sign-on through Dex (RFC 0003 §4): admins by provider group, members
// by invitation or by group, and membership changes taking effect at once.
// The tests build on each other.
test.describe.configure({ mode: "serial" });

const team = `team-${Date.now().toString(36)}`;
let teamId = "";
let alice: { ctx: BrowserContext; page: Page };
let aliceToken = "";

test("an admin logs in through the provider and sets up an account", async ({ page }) => {
  await oidcLogin(page, "admin@example.com");
  const m = await me(page);
  expect(m.body.admin).toBe(true);
  expect(m.body.user.email).toBe("admin@example.com");
  expect(m.body.user.name).toBe("Ada Admin");

  await page.goto("/admin");
  await page.getByRole("button", { name: "Create account" }).click();
  await page.getByRole("dialog").getByLabel("Name").fill(team);
  await page.getByRole("dialog").getByRole("button", { name: "Create" }).click();
  await expect(page.getByRole("link", { name: team })).toBeVisible();
  const accounts = (await (await apiCall("/accounts")).json()).result as { id: string; name: string }[];
  teamId = accounts.find((a) => a.name === team)!.id;

  await page.goto(`/accounts/${teamId}/settings`);
  await page.getByRole("button", { name: "Invite" }).click();
  await page.getByRole("dialog").getByLabel("Email").fill("alice@example.com");
  await page.getByRole("dialog").getByRole("button", { name: "Invite" }).click();
  await expect(page.getByText("Invited", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Edit" }).click();
  await page.getByRole("dialog").getByLabel("Group").fill("team-blue");
  await page.getByRole("dialog").getByRole("button", { name: "Save" }).click();
  await expect(page.getByText("team-blue", { exact: true })).toBeVisible();
});

test("an invited person becomes a member at their first login", async ({ browser }) => {
  const ctx = await browser.newContext({ locale: "en-US" });
  const page = await ctx.newPage();
  alice = { ctx, page };
  await oidcLogin(page, "alice@example.com");
  const m = await me(page);
  expect(m.body.admin).toBe(false);
  expect(m.body.accounts.map((a: { name: string }) => a.name)).toEqual([team]);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(team);
  // Nothing of the platform's: no admin pages, no other account.
  await expect(page.getByRole("link", { name: "Administration" })).toHaveCount(0);
  expect((await page.request.get(`/client/v4/accounts/${env.account}`, { headers: { "x-selvedge-dash": "1" } })).status()).toBe(403);
  expect((await page.request.get("/client/v4/selvedge/users", { headers: { "x-selvedge-dash": "1" } })).status()).toBe(403);

  // A member makes tokens for their tools.
  await page.goto(`/accounts/${teamId}/settings?tab=tokens`);
  await page.getByRole("button", { name: "Create token" }).click();
  await page.getByRole("dialog").getByLabel("Name").fill("alice ci");
  await page.getByRole("dialog").getByRole("button", { name: "Create" }).click();
  aliceToken = (await page.getByText(/^slv_[0-9a-f]+$/).first().textContent())!;
  expect((await apiCall(`/accounts/${teamId}/workers/scripts`, { token: aliceToken })).status).toBe(200);
});

test("the provider's group makes members too", async ({ browser }) => {
  const ctx = await browser.newContext({ locale: "en-US" });
  const page = await ctx.newPage();
  await oidcLogin(page, "bob@example.com");
  const m = await me(page);
  expect(m.body.accounts.map((a: { name: string }) => a.name)).toContain(team);
  await ctx.close();
});

test("removing a member takes effect at once", async () => {
  const members = (await (await apiCall(`/accounts/${teamId}/selvedge/members`)).json()).result as {
    id: string;
    user: { email: string } | null;
  }[];
  const member = members.find((m) => m.user?.email === "alice@example.com")!;
  expect((await apiCall(`/accounts/${teamId}/selvedge/members/${member.id}`, { method: "DELETE" })).status).toBe(200);

  // Her session is still there, but the account is not hers any more.
  const m = await me(alice.page);
  expect(m.status).toBe(200);
  expect(m.body.accounts).toEqual([]);
  const r = await alice.page.request.get(`/client/v4/accounts/${teamId}`, { headers: { "x-selvedge-dash": "1" } });
  expect(r.status()).toBe(403);

  // Her token keeps working (RFC 0003 §12), marked as made by a former member.
  expect((await apiCall(`/accounts/${teamId}/workers/scripts`, { token: aliceToken })).status).toBe(200);
  const tokens = (await (await apiCall(`/accounts/${teamId}/selvedge/tokens`)).json()).result as {
    name: string;
    creator_is_member: boolean | null;
  }[];
  expect(tokens.find((t) => t.name === "alice ci")?.creator_is_member).toBe(false);
});

test("logging out ends the session", async () => {
  const page = alice.page;
  await page.goto(`/`);
  await page.getByRole("button", { name: "Alice Member" }).click();
  await page.getByRole("menuitem", { name: "Log out" }).click();
  await expect(page).toHaveURL(/\/login/);
  expect((await me(page)).status).toBe(401);
  await alice.ctx.close();
});

test("a callback that does not belong to a login is refused", async ({ page }) => {
  await page.goto("/auth/callback?code=forged&state=forged");
  await expect(page).toHaveURL(/\/login\?error=login_expired/);
  await expect(page.getByText("The login took too long or was interrupted.")).toBeVisible();
  expect((await me(page)).status).toBe(401);
});

test("the login page speaks Chinese to Chinese browsers", async ({ browser }) => {
  const ctx = await browser.newContext({ locale: "zh-CN" });
  const page = await ctx.newPage();
  await page.goto("/login");
  await expect(page.getByRole("button", { name: "使用 Dex 登录" })).toBeVisible();
  await expect(page.getByText("Selvedge 是独立项目，与 Cloudflare 无关。")).toBeVisible();
  await ctx.close();
});
