import { DurableObject } from "cloudflare:workers";

export class Counter extends DurableObject {
  async fetch(req) {
    // Holds the request a while, to be in flight when the process stops.
    const ms = Number(new URL(req.url).searchParams.get("ms") ?? 0);
    if (ms > 0) await new Promise((resolve) => setTimeout(resolve, ms));
    const n = ((await this.ctx.storage.get("n")) ?? 0) + 1;
    await this.ctx.storage.put("n", n);
    return new Response(String(n));
  }
  // JS RPC, with SQLite storage.
  async add(k) {
    this.ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS t (v INTEGER)");
    this.ctx.storage.sql.exec("INSERT INTO t VALUES (?)", k);
    return this.ctx.storage.sql.exec("SELECT sum(v) AS s FROM t").one().s;
  }
}

// Memory kept by this process, to push it over its limit.
const kept = [];

export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    if (url.pathname === "/cf") return new Response(JSON.stringify(req.cf));
    if (url.pathname === "/grow") {
      const mb = Number(url.searchParams.get("mb") ?? 64);
      kept.push(new Uint8Array(mb << 20).fill(1));
      return new Response(String(kept.length));
    }
    const stub = env.COUNTER.get(env.COUNTER.idFromName(url.searchParams.get("name") ?? "global"));
    if (url.pathname === "/rpc") return new Response(String(await stub.add(1)));
    return stub.fetch(req);
  },
};
