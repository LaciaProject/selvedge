// Selvedge's tail worker: workerd hands it every script's trace events
// (`tails` on each worker, with the script's name in props). It passes them to
// the agent, which forwards those of scripts someone is tailing. When nobody
// is, the agent answers 204 and this worker stays quiet for a moment, so an
// untailed script costs no I/O per request.
const QUIET_MS = 2000;
const quietUntil = new Map();

export default {
  async tail(events, env, ctx) {
    const script = ctx.props?.script;
    const now = Date.now();
    if (!script || (quietUntil.get(script) ?? 0) > now) return;
    const out = JSON.parse(JSON.stringify(events)).map((e) => {
      e.scriptName = script;
      const req = e.event?.request;
      // The client address the gateway saw. The rest of `cf` describes the
      // runner's local socket (peer PID and UID), not the client.
      if (req) req.cf = { clientIp: req.headers?.["cf-connecting-ip"] ?? "" };
      return e;
    });
    const r = await env.SINK.fetch("http://tail/", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ script, events: out }),
    });
    if (r.status === 204) quietUntil.set(script, now + QUIET_MS);
    else quietUntil.delete(script);
  },
};
