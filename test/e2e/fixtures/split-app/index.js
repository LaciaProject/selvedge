// Answers with its version's VERSION variable, for gradual deployments.
export default {
  fetch(request, env) {
    return new Response(env.VERSION);
  },
};
