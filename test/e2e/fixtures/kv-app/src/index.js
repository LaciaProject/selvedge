export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    if (url.pathname === "/secret") return new Response(env.TOKEN ?? "unset");
    const n = Number((await env.KV.get("count")) ?? 0) + 1;
    await env.KV.put("count", String(n), { metadata: { at: n } });
    return new Response(`${env.GREETING} #${n}`);
  },
};
