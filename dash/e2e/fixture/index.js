// The Worker the dashboard's end-to-end tests deploy: one binding of each kind
// the dashboard shows, and a log line per request for the live logs.
import { DurableObject } from "cloudflare:workers";

export class Counter extends DurableObject {
  async fetch() {
    const n = ((await this.ctx.storage.get("n")) ?? 0) + 1;
    await this.ctx.storage.put("n", n);
    return new Response(String(n));
  }
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    console.log(`dash-app ${request.method} ${url.pathname}`);
    if (url.pathname === "/kv") return new Response((await env.KV.get("greeting")) ?? "none");
    if (url.pathname === "/count") return env.COUNTER.get(env.COUNTER.idFromName("a")).fetch(request);
    if (url.pathname === "/send") {
      await env.QUEUE.send({ at: Date.now() });
      return new Response("sent");
    }
    return new Response(`hello from dash-app (${env.GREETING})`);
  },
  async queue(batch) {
    for (const m of batch.messages) m.ack();
  },
  async scheduled() {},
};
