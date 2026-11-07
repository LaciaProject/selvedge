// Selvedge's D1 backend: the service behind workerd's built-in
// `cloudflare-internal:d1-api` binding module, which speaks HTTP to it:
//
//   POST /query?resultsFormat=ARRAY_OF_OBJECTS|ROWS_AND_COLUMNS|NONE
//        body: {sql, params} or [{sql, params}, ...] (a batch)
//   POST /execute?resultsFormat=NONE   body: {sql, params} (run) or [{sql}, ...] (exec)
//
// Every database is one Durable Object with SQLite storage, so D1 gets the
// cluster's single-writer placement, durability and failover. The database id
// comes from the binding's props, which tenant code cannot change; Selvedge's
// API (and only it) may name a database in a header instead.
import { DurableObject } from "cloudflare:workers";

const toSql = (v) => (Array.isArray(v) ? new Uint8Array(v).buffer : v);
const fromSql = (v) => (v instanceof ArrayBuffer ? Array.from(new Uint8Array(v)) : v);

export class D1Database extends DurableObject {
  run(sql, params, format) {
    const started = Date.now();
    const cursor = this.ctx.storage.sql.exec(sql, ...(params ?? []).map(toSql));
    const columns = cursor.columnNames;
    const rows = cursor.raw().toArray().map((r) => r.map(fromSql));
    const { changes, last } = this.ctx.storage.sql
      .exec("SELECT changes() AS changes, last_insert_rowid() AS last")
      .one();
    const meta = {
      served_by: "selvedge",
      served_by_primary: true,
      served_by_region: "selvedge",
      duration: Date.now() - started,
      timings: { sql_duration_ms: Date.now() - started },
      changes: cursor.rowsWritten > 0 ? changes : 0,
      last_row_id: last,
      changed_db: cursor.rowsWritten > 0,
      size_after: this.ctx.storage.sql.databaseSize,
      rows_read: cursor.rowsRead,
      rows_written: cursor.rowsWritten,
    };
    let results;
    if (format === "ROWS_AND_COLUMNS") results = { columns, rows };
    else if (format === "NONE") results = [];
    else results = rows.map((r) => Object.fromEntries(columns.map((c, i) => [c, r[i]])));
    return { success: true, results, meta };
  }

  // A batch runs in one transaction: all of it or none of it.
  batch(statements, format) {
    return this.ctx.storage.transactionSync(() => statements.map((s) => this.run(s.sql, s.params, format)));
  }

  async fetch(request) {
    const url = new URL(request.url);
    const format = url.searchParams.get("resultsFormat") ?? "ARRAY_OF_OBJECTS";
    try {
      if (request.method !== "POST") return new Response("method not allowed", { status: 405 });
      const body = await request.json();
      if (url.pathname === "/query") {
        return Response.json(Array.isArray(body) ? this.batch(body, format) : this.run(body.sql, body.params, format));
      }
      if (url.pathname === "/execute") {
        // `run()` sends one statement; `exec()` sends one per line.
        if (!Array.isArray(body)) return Response.json(this.run(body.sql, body.params, format));
        const out = [];
        for (const s of body) {
          try {
            out.push(this.run(s.sql, s.params, "NONE"));
          } catch (e) {
            out.push({ success: false, error: e.message });
            break;
          }
        }
        return Response.json(out);
      }
      // The REST API: one SQL string, possibly several statements, in one transaction.
      if (url.pathname === "/rest") {
        const result = this.ctx.storage.transactionSync(() => this.run(body.sql, body.params, "ARRAY_OF_OBJECTS"));
        return Response.json([result]);
      }
      // Database deletion: drop all of its storage.
      if (url.pathname === "/destroy") {
        await this.ctx.storage.deleteAll();
        return Response.json({ success: true });
      }
      return new Response("not found", { status: 404 });
    } catch (e) {
      return Response.json({ success: false, error: e.message });
    }
  }
}

export default {
  fetch(request, env, ctx) {
    const db = ctx.props?.db ?? request.headers.get("x-d1-database");
    if (!db) return new Response("no database", { status: 400 });
    return env.D1.get(env.D1.idFromName(db)).fetch(request);
  },
};
