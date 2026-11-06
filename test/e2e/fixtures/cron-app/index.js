export default {
  async scheduled(event, env) {
    await env.KV.put("cron-last", `${event.cron}|${typeof event.scheduledTime}`);
  },
  async fetch(req, env) {
    return new Response((await env.KV.get("cron-last")) ?? "never");
  },
};
