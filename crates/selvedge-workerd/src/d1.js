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

// A SQL literal for a stored value, as SQLite's `.dump` writes it.
function literal(v) {
  if (v === null || v === undefined) return "NULL";
  if (v instanceof ArrayBuffer) {
    return "X'" + [...new Uint8Array(v)].map((b) => b.toString(16).padStart(2, "0")).join("") + "'";
  }
  if (typeof v === "number" || typeof v === "bigint") return Number.isFinite(Number(v)) ? String(v) : "NULL";
  return "'" + String(v).replaceAll("'", "''") + "'";
}

const ident = (name) => '"' + name.replaceAll('"', '""') + '"';

// How many statements a SQL text holds: semicolons outside quotes and
// comments, plus a trailing statement without one.
function countStatements(sql) {
  let n = 0, pending = false, i = 0;
  while (i < sql.length) {
    const c = sql[i];
    if (c === "'" || c === '"' || c === "`" || c === "[") {
      const end = c === "[" ? "]" : c;
      i++;
      while (i < sql.length && !(sql[i] === end && sql[i + 1] !== end)) i += sql[i] === end ? 2 : 1;
      i++;
      pending = true;
    } else if (c === "-" && sql[i + 1] === "-") {
      while (i < sql.length && sql[i] !== "\n") i++;
    } else if (c === "/" && sql[i + 1] === "*") {
      const end = sql.indexOf("*/", i + 2);
      i = end < 0 ? sql.length : end + 2;
    } else {
      if (c === ";") {
        if (pending) n++;
        pending = false;
      } else if (!/\s/.test(c)) {
        pending = true;
      }
      i++;
    }
  }
  return n + (pending ? 1 : 0);
}

export class D1Database extends DurableObject {
  // `wrangler d1 export`: the schema and rows as SQL, in SQLite's dump format.
  dump({ no_schema, no_data, tables } = {}) {
    const sql = this.ctx.storage.sql;
    const wanted = Array.isArray(tables) && tables.length ? new Set(tables) : null;
    const objects = sql
      .exec(
        `SELECT type, name, tbl_name, sql FROM sqlite_master
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%'
         ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'view' THEN 2 ELSE 1 END, rowid`,
      )
      .toArray()
      .filter((o) => !wanted || wanted.has(o.tbl_name));
    const out = ["PRAGMA defer_foreign_keys=TRUE;"];
    for (const o of objects.filter((o) => o.type === "table")) {
      if (!no_schema) out.push(o.sql + ";");
      if (no_data) continue;
      for (const row of sql.exec(`SELECT * FROM ${ident(o.name)}`).raw()) {
        out.push(`INSERT INTO ${ident(o.name)} VALUES(${row.map(literal).join(",")});`);
      }
    }
    if (!no_schema) for (const o of objects.filter((o) => o.type !== "table")) out.push(o.sql + ";");
    return out.join("\n") + "\n";
  }

  // `wrangler d1 execute --file`: a whole SQL file in one transaction, so a
  // failure leaves the database as it was.
  import(text) {
    const started = Date.now();
    const sql = this.ctx.storage.sql;
    // workerd refuses PRAGMAs a dump starts with; foreign keys are checked
    // at commit regardless.
    const body = text.replace(/^\s*PRAGMA\s+defer_foreign_keys\s*=\s*\w+\s*;/gim, "");
    let read = 0, written = 0;
    this.ctx.storage.transactionSync(() => {
      const cursor = sql.exec(body);
      cursor.toArray();
      read = cursor.rowsRead;
      written = cursor.rowsWritten;
    });
    return {
      num_queries: countStatements(body),
      meta: { duration: Date.now() - started, rows_read: read, rows_written: written, size_after: sql.databaseSize },
    };
  }

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
      if (url.pathname === "/export") return Response.json({ success: true, sql: this.dump(body) });
      if (url.pathname === "/import") return Response.json({ success: true, ...this.import(body.sql ?? "") });
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
