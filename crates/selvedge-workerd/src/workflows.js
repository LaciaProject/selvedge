// Selvedge's Workflows front. Workflow bindings (workerd's own
// `cloudflare-internal:workflows-api`) call its `Binding` entrypoint by
// instance id, with the workflow's name in props; Selvedge's REST API calls
// its fetch handler. It forwards to each workflow's engine (Cloudflare's, in
// vendor/workflows-engine.js: one Durable Object per instance, so instances
// get cluster mode's failover), records instance ids so they can be listed,
// and keeps the engine's local-development fields out of results.
//
// Bindings: B_<workflow> is the engine's WorkflowBinding entrypoint,
// E_<workflow> its Engine namespace, INDEX the instance index namespace.
//
// `details` follows miniflare's local explorer (workers-sdk, MIT), so the
// REST API answers what `wrangler workflows instances describe` expects.
import { DurableObject, WorkerEntrypoint } from "cloudflare:workers";

const STATUS_NAMES = {
  0: "queued", 1: "running", 2: "paused", 3: "errored",
  4: "terminated", 5: "complete", 6: "waitingForPause", 7: "waiting",
};
const EVT = {
  WORKFLOW_QUEUED: 0, WORKFLOW_START: 1, WORKFLOW_SUCCESS: 2, WORKFLOW_FAILURE: 3,
  WORKFLOW_TERMINATED: 4, STEP_START: 5, STEP_SUCCESS: 6, STEP_FAILURE: 7,
  SLEEP_START: 8, SLEEP_COMPLETE: 9, ATTEMPT_START: 10, ATTEMPT_SUCCESS: 11,
  ATTEMPT_FAILURE: 12, WAIT_START: 14, WAIT_COMPLETE: 15, WAIT_TIMED_OUT: 16,
};

// Instance ids of one workflow, newest first.
export class WorkflowIndex extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS instances (id TEXT PRIMARY KEY, created_on TEXT NOT NULL)",
    );
  }
  add(ids) {
    const now = new Date().toISOString();
    for (const id of ids) this.ctx.storage.sql.exec("INSERT OR IGNORE INTO instances VALUES (?, ?)", id, now);
  }
  remove(ids) {
    for (const id of ids) this.ctx.storage.sql.exec("DELETE FROM instances WHERE id = ?", id);
  }
  list(offset, limit) {
    const total = this.ctx.storage.sql.exec("SELECT count(*) AS n FROM instances").one().n;
    const rows = this.ctx.storage.sql
      .exec("SELECT id, created_on FROM instances ORDER BY created_on DESC, id LIMIT ? OFFSET ?", limit, offset)
      .toArray();
    return { total, rows };
  }
}

function binding(env, workflow) {
  const b = env[`B_${workflow}`];
  if (!b) throw new Error("workflow.not_found");
  return b;
}
const index = (env, workflow) => env.INDEX.get(env.INDEX.idFromName(workflow));
const ids = (r) => (Array.isArray(r) ? r : r?.created ?? []).map((x) => x.id);
function clean(status) {
  if (status && typeof status === "object") {
    const { __LOCAL_DEV_STEP_OUTPUTS, ...rest } = status;
    return rest;
  }
  return status;
}

// What workerd's workflows-api calls, by instance id.
export class Binding extends WorkerEntrypoint {
  get #wf() {
    return this.ctx.props.workflow;
  }
  async create(options) {
    const r = await binding(this.env, this.#wf).create(options);
    await index(this.env, this.#wf).add([r.id]);
    return r;
  }
  async createBatch(options) {
    const r = await binding(this.env, this.#wf).createBatch(options);
    await index(this.env, this.#wf).add(ids(r));
    return r;
  }
  async deleteBatch(options) {
    const r = await binding(this.env, this.#wf).deleteBatch(options);
    await index(this.env, this.#wf).remove(options?.instances ?? []);
    return r;
  }
  async deleteInstance(id) {
    await binding(this.env, this.#wf).deleteInstance(id);
    await index(this.env, this.#wf).remove([id]);
  }
  getInstance(id) { return binding(this.env, this.#wf).getInstance(id); }
  pause(id) { return binding(this.env, this.#wf).pause(id); }
  resume(id) { return binding(this.env, this.#wf).resume(id); }
  terminate(id, o) { return binding(this.env, this.#wf).terminate(id, o); }
  restart(id, o) { return binding(this.env, this.#wf).restart(id, o); }
  sendEvent(id, e) { return binding(this.env, this.#wf).sendEvent(id, e); }
  subscribe(id, o) { return binding(this.env, this.#wf).subscribe(id, o); }
  async status(id) {
    return clean(await binding(this.env, this.#wf).status(id));
  }
}

async function details(env, workflow, id) {
  const ns = env[`E_${workflow}`];
  if (!ns) return null;
  const stub = ns.get(ns.idFromName(id));
  let metadata;
  try {
    metadata = await stub.getInstanceMetadata();
  } catch {
    return null;
  }
  if (!metadata.instanceId) return null;
  const logs = await stub.readDetailedLogs();
  const find = (e) => logs.find((l) => l.event === e);
  const queued = find(EVT.WORKFLOW_QUEUED), start = find(EVT.WORKFLOW_START);
  const success = find(EVT.WORKFLOW_SUCCESS), failure = find(EVT.WORKFLOW_FAILURE);
  const end = success ?? failure ?? find(EVT.WORKFLOW_TERMINATED);
  const groups = new Map();
  for (const l of logs) {
    if (l.group) groups.set(l.group, [...(groups.get(l.group) ?? []), l]);
  }
  const steps = [];
  for (const g of groups.values()) {
    const name = g[0].target ?? "";
    const one = (e) => g.find((l) => l.event === e);
    const all = (e) => g.filter((l) => l.event === e);
    const sleep = one(EVT.SLEEP_START), wait = one(EVT.WAIT_START), step = one(EVT.STEP_START);
    if (sleep) {
      const done = one(EVT.SLEEP_COMPLETE);
      steps.push({ name, type: "sleep", start: sleep.timestamp, end: done?.timestamp ?? null, finished: !!done, error: null });
    } else if (wait) {
      const ok = one(EVT.WAIT_COMPLETE), timedOut = one(EVT.WAIT_TIMED_OUT), stop = ok ?? timedOut;
      steps.push({
        name, type: "waitForEvent", start: wait.timestamp, end: stop?.timestamp ?? null, finished: !!stop,
        error: timedOut?.metadata ? { name: timedOut.metadata.name ?? "Error", message: timedOut.metadata.message ?? "" } : null,
        output: ok ? { type: ok.metadata?.type ?? "", payload: ok.metadata?.payload ?? {}, timestamp: ok.metadata?.timestamp ?? null } : null,
      });
    } else if (step) {
      const attempts = all(EVT.ATTEMPT_START).map((a) => {
        const n = a.metadata.attempt;
        const ok = all(EVT.ATTEMPT_SUCCESS).find((l) => l.metadata.attempt === n);
        const bad = all(EVT.ATTEMPT_FAILURE).find((l) => l.metadata.attempt === n);
        return {
          start: a.timestamp, end: (ok ?? bad)?.timestamp ?? null,
          success: ok ? true : bad ? false : null, error: bad ? bad.metadata.error ?? null : null,
        };
      });
      const ok = one(EVT.STEP_SUCCESS), bad = one(EVT.STEP_FAILURE);
      steps.push({
        name, type: "step", start: step.timestamp, end: (ok ?? bad)?.timestamp ?? null,
        success: ok ? true : bad ? false : null, output: ok?.metadata?.result, config: step.metadata?.config ?? null, attempts,
      });
    }
  }
  return {
    status: STATUS_NAMES[metadata.status] ?? "unknown",
    params: queued?.metadata?.params ?? null,
    trigger: { source: "api" },
    queued: queued?.timestamp ?? null,
    start: start?.timestamp ?? null,
    end: end?.timestamp ?? null,
    success: success ? true : failure ? false : null,
    output: success?.metadata?.result ?? null,
    error: failure ? failure.metadata.error ?? null : null,
    steps,
    step_count: steps.length,
  };
}

const json = (v, status = 200) => Response.json(v, { status });
const err = (status, message) => json({ error: message }, status);

// The REST API: POST /<op> with {workflow, ...}. Only platform components
// reach it, through the gateway's internal routing.
export default {
  async fetch(request, env) {
    const op = new URL(request.url).pathname.slice(1);
    const body = await request.json();
    const wf = body.workflow;
    if (!env[`B_${wf}`]) return err(404, "workflow.not_found");
    const b = binding(env, wf);
    try {
      switch (op) {
        case "create": {
          // The engine picks an id only when `id` is absent (not null).
          const r = await b.create(body.id ? { id: body.id, params: body.params } : { params: body.params });
          await index(env, wf).add([r.id]);
          return json({ id: r.id });
        }
        case "details": {
          const d = await details(env, wf, body.id);
          return d ? json(d) : err(404, "instance.not_found");
        }
        case "list": {
          const { total, rows } = await index(env, wf).list(body.offset ?? 0, body.limit ?? 50);
          const instances = await Promise.all(
            rows.map(async (r) => {
              let status;
              try {
                status = STATUS_NAMES[(await env[`E_${wf}`].get(env[`E_${wf}`].idFromName(r.id)).getInstanceMetadata()).status];
              } catch {}
              return { id: r.id, created_on: r.created_on, modified_on: r.created_on, ...(status ? { status } : {}) };
            }),
          );
          return json({ total, instances });
        }
        case "status": {
          const { id, status, options } = body;
          if (status === "pause") await b.pause(id);
          else if (status === "resume") await b.resume(id);
          else if (status === "terminate") await b.terminate(id, options);
          else if (status === "restart") await b.restart(id, options);
          else return err(400, `unknown status action ${status}`);
          return json({ ok: true });
        }
        case "delete": {
          const r = await b.deleteBatch({ instances: body.instances ?? [] });
          await index(env, wf).remove((r?.deleted ?? []).map((x) => x.id));
          return json(r);
        }
        case "event":
          await b.sendEvent(body.id, { type: body.type, payload: body.payload });
          return json({ ok: true });
        default:
          return err(404, "not found");
      }
    } catch (e) {
      const message = String(e?.message ?? e);
      return err(message.includes("not_found") ? 404 : 400, message);
    }
  },
};
