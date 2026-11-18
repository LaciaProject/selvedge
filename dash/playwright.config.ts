import { defineConfig } from "@playwright/test";

// Run by test/e2e/dash.sh, which starts `selvedge all` with the dashboard and
// Dex, seeds an account, and passes where they are in the environment.
export default defineConfig({
  testDir: "e2e",
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [["list"]],
  use: {
    baseURL: process.env.DASH_URL ?? "http://localhost:8790",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    // The main flows in each language (RFC 0003 §8).
    { name: "en", use: { locale: "en-US" }, testIgnore: /oidc/ },
    { name: "zh-CN", use: { locale: "zh-CN" }, testIgnore: /oidc|security/ },
    { name: "oidc", use: { locale: "en-US" }, testMatch: /oidc/ },
  ],
});
