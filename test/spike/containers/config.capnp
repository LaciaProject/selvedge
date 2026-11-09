using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "main", worker = .worker),
  ],
  sockets = [
    (name = "http", address = "127.0.0.1:18490", http = (), service = "main"),
  ],
);

const worker :Workerd.Worker = (
  modules = [(name = "worker.js", esModule = embed "worker.js")],
  compatibilityDate = "2026-10-01",
  compatibilityFlags = ["enable_ctx_exports"],
  containerEngine = (
    localDocker = (
      socketPath = "unix:/var/run/docker.sock",
      containerEgressInterceptorImage = "cloudflare/proxy-everything:main",
    ),
  ),
  durableObjectNamespaces = [
    (className = "Box", uniqueKey = "spike-box", container = (imageName = "selvedge-spike-app:1")),
  ],
  durableObjectStorage = (inMemory = void),
  bindings = [(name = "BOX", durableObjectNamespace = "Box")],
);
