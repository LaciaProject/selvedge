// Reached through zone routes.
export default {
  fetch(request) {
    const url = new URL(request.url);
    return new Response(`route:${url.hostname}${url.pathname}`);
  },
};
