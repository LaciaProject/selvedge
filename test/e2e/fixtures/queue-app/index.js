export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    if (url.pathname === "/send") {
      await env.JOBS.send({ n: 1 });
      await env.JOBS.sendBatch([{ body: { n: 2 } }, { body: "three", contentType: "text" }, { body: { flaky: true } }]);
      return new Response("sent");
    }
    return new Response((await env.KV.get("q-log")) ?? "");
  },
  async queue(batch, env) {
    let log = (await env.KV.get("q-log")) ?? "";
    for (const m of batch.messages) {
      if (m.body?.flaky && m.attempts < 2) { m.retry(); continue; }
      log += `${batch.queue}:${JSON.stringify(m.body)}#${m.attempts};`;
      m.ack();
    }
    await env.KV.put("q-log", log);
  },
};
