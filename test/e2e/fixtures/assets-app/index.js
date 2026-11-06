// A Worker with static assets: /api/* runs the Worker first; everything else
// is an asset when one matches.
export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/api/cf") return Response.json(request.cf);
    if (url.pathname === "/api/boom") throw new Error("boom from the worker");
    console.log("handled", url.pathname);
    if (url.pathname === "/api/asset") {
      const css = await env.ASSETS.fetch(new URL("/style.css", request.url));
      return new Response(`asset:${css.status}:${(await css.text()).trim()}`);
    }
    return new Response(`worker:${url.pathname}`);
  },
};
