// A Workflow with a step, a wait for an event and a sleep, driven through
// its binding.
import { WorkflowEntrypoint } from "cloudflare:workers";

export class Flow extends WorkflowEntrypoint {
  async run(event, step) {
    const doubled = await step.do("double", async () => event.payload.n * 2);
    const approval = await step.waitForEvent("approval", { type: "approve", timeout: "10 minutes" });
    await step.sleep("pause", "1 second");
    return { doubled, approved: approval.payload.ok };
  }
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const id = url.searchParams.get("id");
    if (url.pathname === "/start") {
      const instance = await env.FLOW.create({ params: { n: Number(url.searchParams.get("n") ?? 1) } });
      return new Response(instance.id);
    }
    const instance = await env.FLOW.get(id);
    if (url.pathname === "/approve") {
      await instance.sendEvent({ type: "approve", payload: { ok: true } });
      return new Response("approved");
    }
    return Response.json(await instance.status());
  },
};
