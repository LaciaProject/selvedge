import { DurableObject } from "cloudflare:workers";
export class Room extends DurableObject {
  async fetch(req) {
    const url = new URL(req.url);
    if (url.pathname === "/ws") {
      const [client, server] = Object.values(new WebSocketPair());
      this.ctx.acceptWebSocket(server);
      return new Response(null, { status: 101, webSocket: client });
    }
    if (url.pathname === "/alarm") { await this.ctx.storage.setAlarm(Date.now() + 1000); return new Response("set"); }
    return new Response(String((await this.ctx.storage.get("alarms")) ?? 0));
  }
  async webSocketMessage(ws, msg) { ws.send(`echo:${msg}`); }
  async alarm() { await this.ctx.storage.put("alarms", ((await this.ctx.storage.get("alarms")) ?? 0) + 1); }
}
export default { fetch(req, env) { return env.ROOM.get(env.ROOM.idFromName("r")).fetch(req); } };
