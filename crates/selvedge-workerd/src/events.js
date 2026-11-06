// Selvedge's event dispatcher: delivers Cron Triggers to an account's
// scripts. Only platform components reach it, through the gateway's internal
// routing; each script is bound as `S_<script name>`.
//
// The handler is invoked over the service binding with an event shaped like
// Cloudflare's ScheduledController. (workerd's Fetcher.scheduled() would need
// the experimental `service_binding_extra_handlers` flag, which Selvedge does
// not enable, since tenants could then turn on experimental flags too.)
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
    return new Response("not found", { status: 404 });
  },
};
