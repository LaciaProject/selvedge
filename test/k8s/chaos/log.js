// An append-only log per Durable Object, for the consistency check under
// faults (test/k8s/chaos.sh). `?key=<name>&append=<token>` adds a token and
// answers the whole log after it; `?key=<name>` only reads it. Every answer is
// the log as of one moment in the object's single order of operations, so a
// history of answers can be checked for linearizability (check.py).
import { DurableObject } from "cloudflare:workers";

export class Log extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS log (seq INTEGER PRIMARY KEY AUTOINCREMENT, token TEXT NOT NULL)",
    );
  }

  async fetch(req) {
    const token = new URL(req.url).searchParams.get("append");
    if (token) this.ctx.storage.sql.exec("INSERT INTO log (token) VALUES (?)", token);
    const tokens = this.ctx.storage.sql
      .exec("SELECT token FROM log ORDER BY seq")
      .toArray()
      .map((row) => row.token);
    // The output gate holds this answer until the insert is durable.
    return new Response(tokens.join("\n"));
  }
}

export default {
  async fetch(req, env) {
    const key = new URL(req.url).searchParams.get("key");
    if (!key) return new Response("missing key", { status: 400 });
    return env.LOG.get(env.LOG.idFromName(key)).fetch(req);
  },
};
