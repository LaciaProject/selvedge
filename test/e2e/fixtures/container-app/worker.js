// A Durable Object with a container. `/<step>?name=<object>` runs one step
// against the object's container and answers with text.
import { DurableObject, WorkerEntrypoint } from "cloudflare:workers";

export class Echo extends WorkerEntrypoint {
  fetch(req) {
    return new Response(`intercepted ${new URL(req.url).pathname}`);
  }
}

export class Box extends DurableObject {
  async up(options) {
    const c = this.ctx.container;
    let failure;
    if (!c.running) {
      c.start(options);
      // start() does not report failures; monitor() does.
      c.monitor().catch((e) => (failure = e));
      // Without this the container stops when the object is evicted from
      // memory, some seconds after its last request.
      await c.setInactivityTimeout(10 * 60 * 1000);
    }
    // Until the application answers on its port.
    for (let i = 0; i < 1200 && !failure; i++) {
      try {
        const r = await c.getTcpPort(8080).fetch("http://container/");
        if (r.ok) return await r.text();
      } catch {}
      await new Promise((r) => setTimeout(r, 100));
    }
    throw failure ?? new Error("the container did not answer within 120 s");
  }

  inside(path) {
    return this.ctx.container.getTcpPort(8080).fetch(`http://container${path}`);
  }

  async fetch(req) {
    const url = new URL(req.url);
    const q = url.searchParams;
    const c = this.ctx.container;
    try {
      switch (url.pathname) {
        case "/start": {
          const options = { env: { MESSAGE: q.get("msg") ?? "nobody" } };
          if (q.get("internet") === "1") options.enableInternet = true;
          if (q.get("args")) options.entrypoint = ["python3", "-u", "/server.py", q.get("args")];
          if (q.get("image")) options.image = q.get("image");
          return new Response(await this.up(options));
        }
        case "/hello":
          return await this.inside("/");
        case "/ca":
          return await this.inside("/ca");
        case "/args":
          return await this.inside("/args");
        case "/out":
          return await this.inside(`/out?u=${encodeURIComponent(q.get("u"))}`);
        case "/intercept":
          await c.interceptOutboundHttp("intercepted.test", this.ctx.exports.Echo({}));
          return await this.inside(`/out?u=${encodeURIComponent("http://intercepted.test/x")}`);
        case "/intercept-https":
          await c.interceptOutboundHttps("secure.test", this.ctx.exports.Echo({}));
          return await this.inside(`/out?u=${encodeURIComponent("https://secure.test/y")}`);
        case "/running":
          return new Response(String(c.running));
        case "/exit": {
          const done = c.monitor().then(
            () => "exited 0",
            (e) => `exited: ${e.message}`,
          );
          await this.inside(`/exit?code=${q.get("code") ?? 0}`);
          return new Response(await done);
        }
        case "/signal":
          c.signal(Number(q.get("s")));
          return new Response("sent");
        case "/destroy":
          await c.destroy();
          return new Response("destroyed");
        case "/exec": {
          const p = await c.exec(["true"]);
          return new Response(`exec ${await p.exitCode}`);
        }
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
    const name = new URL(req.url).searchParams.get("name") ?? "a";
    return env.BOX.getByName(name).fetch(req);
  },
};
