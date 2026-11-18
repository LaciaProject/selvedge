import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vitest/config";

// `npm run dev` proxies the dashboard's own paths to a `selvedge all` started
// with --dash-listen (SELVEDGE_DASH, default http://localhost:8790), so the
// browser sees one origin, as in production.
const target = process.env.SELVEDGE_DASH ?? "http://localhost:8790";
const proxy = { target, changeOrigin: false, ws: true };

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    proxy: { "/auth": proxy, "/client/v4": proxy, "/tail": proxy },
  },
  build: {
    // The API serves these with its own CSP: no inline scripts.
    assetsInlineLimit: 0,
    sourcemap: false,
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
