// Selvedge's event dispatcher: delivers Cron Triggers and queue batches to
// an account's scripts. Only platform components reach it, through the gateway's internal
// routing; each script is bound as `S_<script name>`.
//
// Handlers are invoked over the service binding with an event shaped like
// Cloudflare's ScheduledController or MessageBatch. (workerd's Fetcher.scheduled() would need
// the experimental `service_binding_extra_handlers` flag, which Selvedge does
// not enable, since tenants could then turn on experimental flags too.)
const fromBase64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

function decode(m) {
  const bytes = fromBase64(m.body);
  if (m.contentType === "bytes") return bytes.buffer;
  const text = new TextDecoder().decode(bytes);
  return m.contentType === "text" ? text : JSON.parse(text);
}

async function deliverBatch(target, queue, raw) {
  const acks = new Set();
  const retries = {};
  let ackAll = false;
  let retryAll = null;
  const delayOf = (o) => (o && typeof o.delaySeconds === "number" ? o.delaySeconds : null);
  const batch = {
    queue,
    messages: raw.map((m) => ({
      id: m.id,
      timestamp: new Date(m.timestamp),
      attempts: m.attempts,
      body: decode(m),
      ack() { acks.add(m.id); delete retries[m.id]; },
      retry(o) { retries[m.id] = delayOf(o); acks.delete(m.id); },
    })),
    ackAll() { ackAll = true; },
    retryAll(o) { retryAll = delayOf(o) ?? 0; },
  };
  let outcome = "ok";
  let error;
  try {
    await target.queue(batch);
  } catch (e) {
    outcome = "exception";
    error = String(e?.message ?? e);
  }
  return { outcome, error, acks: [...acks], retries, ackAll, retryAll };
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const body = await request.json();
    const target = env[`S_${body.script}`];
    if (!target) return new Response("no such script", { status: 404 });
    if (url.pathname === "/scheduled") {
      try {
        await target.scheduled({ type: "scheduled", scheduledTime: body.scheduledTime, cron: body.cron, noRetry() {} });
        return Response.json({ outcome: "ok" });
      } catch (e) {
        return Response.json({ outcome: "exception", error: String(e?.message ?? e) });
      }
    }
    if (url.pathname === "/queue") {
      return Response.json(await deliverBatch(target, body.queue, body.messages));
    }
    return new Response("not found", { status: 404 });
  },
};
