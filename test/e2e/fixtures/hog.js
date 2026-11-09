// Uses as much memory or CPU as it is asked to, or hangs, to test per-account
// limits and hang detection.
const kept = [];
let served = 0;
export default {
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === "/grow") {
      kept.push(new Uint8Array(64 << 20).fill(1));
      return new Response(String(kept.length));
    }
    if (path === "/hang") {
      for (;;) {}
    }
    if (path === "/spin") {
      const end = Date.now() + 1000;
      while (Date.now() < end) {}
      return new Response("spun");
    }
    // Counts requests served by this process: 1 again after a restart.
    return new Response(String(++served));
  },
};
