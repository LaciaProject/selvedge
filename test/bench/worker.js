// The benchmark Worker: `/` answers from the isolate, `/kv` reads a KV key
// (a binding call through the agent to the data service), `/do` calls one
// Durable Object (a hot object: its requests run one at a time).
import { DurableObject } from "cloudflare:workers";

export class Bench extends DurableObject {
  async fetch() {
    const n = ((await this.ctx.storage.get("n")) ?? 0) + 1;
    this.ctx.storage.put("n", n);
    return new Response(String(n));
  }
}

export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    if (url.pathname === "/kv") return new Response((await env.KV.get("k")) ?? "missing");
    if (url.pathname === "/do") {
      const name = url.searchParams.get("o") ?? "bench";
      return env.BENCH.get(env.BENCH.idFromName(name)).fetch(req);
    }
    return new Response("ok");
  },
};
