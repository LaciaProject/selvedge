// Exercises the D1 binding; GET /run returns the results as JSON.
export default {
  async fetch(req, env) {
    const db = env.DB;
    const out = {};
    await db.exec("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, avatar BLOB)");
    await db.exec("DELETE FROM users");
    const ins = await db.prepare("INSERT INTO users (name, avatar) VALUES (?1, ?2)").bind("ada", new Uint8Array([1, 2, 3])).run();
    out.insert = [ins.success, ins.meta.changes, ins.meta.last_row_id > 0];
    out.first = await db.prepare("SELECT name FROM users WHERE id = ?").bind(ins.meta.last_row_id).first("name");
    out.blob = (await db.prepare("SELECT avatar FROM users").first()).avatar;
    const batch = await db.batch([
      db.prepare("INSERT INTO users (name) VALUES (?)").bind("grace"),
      db.prepare("INSERT INTO users (name) VALUES (?)").bind("linus"),
      db.prepare("SELECT count(*) AS n FROM users"),
    ]);
    out.batch = batch[2].results[0].n;
    out.all = (await db.prepare("SELECT name FROM users ORDER BY name").all()).results.map((r) => r.name);
    out.raw = await db.prepare("SELECT id, name FROM users ORDER BY id LIMIT 1").raw({ columnNames: true });
    // A failing batch rolls back entirely.
    try {
      await db.batch([db.prepare("INSERT INTO users (name) VALUES ('ghost')"), db.prepare("INSERT INTO nope VALUES (1)")]);
      out.rollback = "no error";
    } catch (e) {
      out.rollback = (await db.prepare("SELECT count(*) AS n FROM users WHERE name = 'ghost'").first("n")) === 0;
    }
    try {
      await db.prepare("SELECT * FROM missing_table").all();
    } catch (e) {
      out.error = e.message.startsWith("D1_ERROR");
    }
    return Response.json(out);
  },
};
