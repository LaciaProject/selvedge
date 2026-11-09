// A Durable Object with a container. `/<step>?name=<object>` runs one step and
// answers with text; run.sh times and records them.
import { DurableObject, WorkerEntrypoint } from "cloudflare:workers";

export class Echo extends WorkerEntrypoint {
  fetch(req) {
    return new Response(`intercepted ${req.method} ${req.url}`);
  }
}

export class Box extends DurableObject {
  async ready(internet) {
    const c = this.ctx.container;
    if (!c.running) c.start(internet ? { enableInternet: true } : {});
    // Wait until the app answers on its port.
    for (let i = 0; i < 600; i++) {
      try {
        const r = await c.getTcpPort(8080).fetch("http://container/");
        if (r.ok) return await r.text();
      } catch {}
      await new Promise((r) => setTimeout(r, 100));
    }
    throw new Error("the container did not answer within 60 s");
  }

  async fetch(req) {
    const url = new URL(req.url);
    const c = this.ctx.container;
    try {
      switch (url.pathname) {
        case "/ready":
          return new Response(await this.ready(url.searchParams.get("internet") === "1"));
        case "/hello":
          return c.getTcpPort(8080).fetch("http://container/");
        case "/out":
          return c.getTcpPort(8080).fetch(`http://container/out?u=${encodeURIComponent(url.searchParams.get("u"))}`);
        case "/intercept":
          await c.interceptOutboundHttp("intercepted.test", this.ctx.exports.Echo({}));
          return c.getTcpPort(8080).fetch(`http://container/out?u=${encodeURIComponent("http://intercepted.test/x")}`);
        case "/destroy":
          await c.destroy();
          return new Response("destroyed");
        default:
          return new Response("not found", { status: 404 });
      }
    } catch (e) {
      return new Response(`error ${e}`, { status: 500 });
    }
  }
}

export default {
  fetch(req, env) {
    const name = new URL(req.url).searchParams.get("name") ?? "box";
    return env.BOX.getByName(name).fetch(req);
  },
};
