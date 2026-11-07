import { DurableObject } from "cloudflare:workers";

export class Counter extends DurableObject {
  async fetch(req) {
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

export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    const stub = env.COUNTER.get(env.COUNTER.idFromName(url.searchParams.get("name") ?? "global"));
    if (url.pathname === "/rpc") return new Response(String(await stub.add(1)));
    return stub.fetch(req);
  },
};
