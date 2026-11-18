import { expect, test } from "@playwright/test";

import { apiCall, env, tokenLogin, tr, workerGet, workerHost } from "./helpers";

// The main flows, logged in with the seeded account's token. Each language's
// project runs them; what they create carries the language in its name.

test.beforeEach(async ({ page }, info) => {
  await tokenLogin(page, info);
});

const base = () => `/accounts/${env.account}`;
const suffix = (name: string) => name.replace(/[^a-z]/gi, "").toLowerCase();

test("the overview counts what the account has", async ({ page }, info) => {
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("dash");
  await expect(page.getByRole("link", { name: "dash-app" })).toBeVisible();
  await expect(page.getByText(tr(info, "overview.recentDeployments"))).toBeVisible();
});

test("a Worker's versions, bindings and triggers", async ({ page }, info) => {
  await page.goto(`${base()}/workers/dash-app`);
  // Two deploys and a secret make three versions at least; the newest is live.
  await expect(page.getByRole("meter")).toContainText(tr(info, "workers.percent", { value: 100 }));
  expect(await page.getByRole("row").count()).toBeGreaterThanOrEqual(4);
  await page.getByRole("tab", { name: tr(info, "workers.tabs.settings") }).click();
  for (const binding of ["KV", "DB", "BUCKET", "QUEUE", "COUNTER"]) {
    await expect(page.getByRole("cell", { name: binding, exact: true }).first()).toBeVisible();
  }
  await page.getByRole("tab", { name: tr(info, "workers.tabs.triggers") }).click();
  await expect(page.getByText("*/30 * * * *")).toBeVisible();
  await expect(page.getByText(`https://${workerHost()}`)).toBeVisible();
});

test("live logs show a request as it happens", async ({ page }, info) => {
  await page.goto(`${base()}/workers/dash-app?tab=logs`);
  await page.getByRole("button", { name: tr(info, "logs.start") }).click();
  await expect(page.getByText(tr(info, "logs.state.live"), { exact: true })).toBeVisible();
  const path = `/from-e2e-${suffix(info.project.name)}`;
  // The tail session reaches the runner within a couple of seconds.
  await expect(async () => {
    expect(await workerGet(path)).toBe(200);
    await expect(page.getByTestId("log-event").filter({ hasText: `dash-app GET ${path}` }).first()).toBeVisible({
      timeout: 2000,
    });
  }).toPass({ timeout: 30_000 });
  await page.getByRole("button", { name: tr(info, "logs.stop") }).click();
  await expect(page.getByText(tr(info, "logs.state.idle"), { exact: true })).toBeVisible();
});

test("a secret is added and deleted", async ({ page }, info) => {
  const name = `E2E_${suffix(info.project.name).toUpperCase()}`;
  await page.goto(`${base()}/workers/dash-app?tab=settings`);
  await page.getByRole("button", { name: tr(info, "workers.addSecret") }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel(tr(info, "common.name")).fill(name);
  await dialog.getByLabel(tr(info, "workers.secretValue")).fill("shh");
  await dialog.getByRole("button", { name: tr(info, "common.save") }).click();
  await expect(page.getByRole("cell", { name, exact: true })).toBeVisible();
  await page.getByRole("button", { name: tr(info, "workers.deleteSecret", { name }) }).click();
  await page.getByRole("dialog").getByRole("textbox").fill(name);
  await page.getByRole("dialog").getByRole("button", { name: tr(info, "confirm.delete", { kind: tr(info, "kinds.secret") }) }).click();
  await expect(page.getByRole("cell", { name, exact: true })).toHaveCount(0);
});

test("a KV value is read and changed", async ({ page }, info) => {
  await page.goto(`${base()}/kv`);
  await page.getByRole("link", { name: "dash-kv" }).click();
  await page.getByRole("button", { name: "greeting" }).click();
  const value = page.getByRole("dialog").getByLabel(tr(info, "kv.value"));
  await expect(value).toHaveValue(/hello/);
  const next = `hello ${info.project.name}`;
  await value.fill(next);
  await page.getByRole("dialog").getByLabel(tr(info, "kv.metadata")).fill(`{"by": "${info.project.name}"}`);
  await page.getByRole("dialog").getByRole("button", { name: tr(info, "common.save") }).click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  const ns = (await (await apiCall(`/accounts/${env.account}/storage/kv/namespaces`, { token: env.token })).json()).result[0].id;
  const got = await apiCall(`/accounts/${env.account}/storage/kv/namespaces/${ns}/values/greeting`, { token: env.token });
  expect(await got.text()).toBe(next);
  const meta = await apiCall(`/accounts/${env.account}/storage/kv/namespaces/${ns}/metadata/greeting`, { token: env.token });
  expect((await meta.json()).result).toEqual({ by: info.project.name });
});

test("SQL runs against a D1 database", async ({ page }, info) => {
  await page.goto(`${base()}/d1`);
  await page.getByRole("link", { name: "dash-db" }).click();
  await page.getByLabel(tr(info, "d1.query")).fill("SELECT body FROM notes ORDER BY id");
  await page.getByRole("button", { name: tr(info, "d1.run") }).click();
  await expect(page.getByRole("cell", { name: "first" })).toBeVisible();
  await expect(page.getByRole("cell", { name: "second" })).toBeVisible();
  await page.getByRole("tab", { name: tr(info, "d1.tabs.tables") }).click();
  await expect(page.getByRole("cell", { name: "notes", exact: true })).toBeVisible();
});

test("files are uploaded to R2, listed and deleted", async ({ page }, info) => {
  const name = `e2e-${suffix(info.project.name)}.txt`;
  await page.goto(`${base()}/r2/dash-bucket`);
  await page.locator('input[type="file"]').setInputFiles({ name, mimeType: "text/plain", buffer: Buffer.from("from the dashboard") });
  await expect(page.getByText(name)).toBeVisible();
  await expect(page.getByText("text/plain").first()).toBeVisible();
  await page.getByRole("button", { name: tr(info, "r2.deleteObject", { name }) }).click();
  await page.getByRole("dialog").getByRole("textbox").fill(name);
  await page.getByRole("dialog").getByRole("button", { name: tr(info, "confirm.delete", { kind: tr(info, "kinds.object") }) }).click();
  await expect(page.getByText(name)).toHaveCount(0);
});

test("a message is sent to a queue", async ({ page }, info) => {
  await page.goto(`${base()}/queues`);
  await page.getByRole("link", { name: "dash-queue" }).click();
  await expect(page.getByText("dash-app").first()).toBeVisible();
  await page.getByRole("button", { name: tr(info, "queues.send") }).click();
  await page.getByRole("dialog").getByRole("button", { name: tr(info, "queues.sendButton") }).click();
  await expect(page.getByText(tr(info, "queues.sent"))).toBeVisible();
});

test("an account token's session does not manage tokens", async ({ page }, info) => {
  await page.goto(`${base()}/settings?tab=tokens`);
  await expect(page.getByText(tr(info, "settings.tokensNeedPerson"))).toBeVisible();
  await expect(page.getByRole("button", { name: tr(info, "settings.createToken") })).toHaveCount(0);
});

test("a token is created, works, and is revoked", async ({ page }, info) => {
  const name = `e2e ${info.project.name}`;
  // People and admins manage tokens: log in with the admin token.
  await tokenLogin(page, info, env.adminToken);
  await page.goto(`${base()}/settings?tab=tokens`);
  await page.getByRole("button", { name: tr(info, "settings.createToken") }).click();
  await page.getByRole("dialog").getByLabel(tr(info, "common.name")).fill(name);
  await page.getByRole("dialog").getByRole("button", { name: tr(info, "common.create") }).click();
  await expect(page.getByText(tr(info, "settings.tokenOnce"))).toBeVisible();
  const value = await page.getByText(/^slv_[0-9a-f]+$/).first().textContent();
  expect(value).toMatch(/^slv_/);
  // An account token cannot make tokens, but it works for the account.
  expect((await apiCall(`/accounts/${env.account}/workers/scripts`, { token: value! })).status).toBe(200);
  await page.getByRole("button", { name: tr(info, "settings.tokenSaved") }).click();
  await page.getByRole("button", { name: tr(info, "settings.revoke", { name }) }).click();
  await page.getByRole("dialog").getByRole("textbox").fill(name);
  await page.getByRole("dialog").getByRole("button", { name: tr(info, "settings.revokeButton") }).click();
  await expect(page.getByRole("cell", { name, exact: true })).toHaveCount(0);
  expect((await apiCall(`/accounts/${env.account}/workers/scripts`, { token: value! })).status).toBe(401);
});

test("the language can be switched", async ({ page }, info) => {
  const other = info.project.name === "zh-CN" ? "English" : "简体中文";
  await page.getByRole("button", { name: tr(info, "nav.language") }).click();
  await page.getByRole("menuitem", { name: other }).click();
  await expect(page.getByRole("link", { name: other === "English" ? "Overview" : "概览" })).toBeVisible();
});
