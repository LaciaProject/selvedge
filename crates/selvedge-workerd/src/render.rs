//! Renders one account's bundle as an official workerd configuration (Cap'n Proto
//! text format). Every module is written next to the config and loaded with
//! `embed`, so module bytes never need escaping.
//!
//! Layout of the rendered process:
//!
//! - one worker service per script (`w-<script>`), each with its own Unix socket;
//! - one external service per KV namespace (`kv-<id>`), pointing at the agent's
//!   binding socket for this account, with the namespace id injected as a header
//!   the tenant cannot change;
//! - `internet`, the only outbound network, limited to public addresses;
//! - `selvedge-missing`, which answers for service bindings whose target script is
//!   not deployed, so one missing script cannot stop the whole account;
//! - for a script with static assets, Cloudflare's asset worker (`assets-<script>`)
//!   and router worker (`router-<script>`). The script's socket points at the
//!   router, which serves an asset or calls the script (`w-<script>`, absent for
//!   a Worker that only has assets). Asset bytes come from `selvedge-assets-kv`,
//!   a KV-protocol service on the agent's binding socket;
//! - `selvedge-health`, which the agent probes. All of a process's Workers run
//!   on one thread, so a busy loop in tenant code stops it answering, too.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use selvedge_core::{AccountBundle, Binding, BundleScript, ModuleKind, WorkflowDef};

/// The header carrying the KV namespace id on binding requests.
pub const KV_NAMESPACE_HEADER: &str = "x-selvedge-kv-namespace";
/// The header carrying the R2 bucket name on binding requests.
pub const R2_BUCKET_HEADER: &str = "x-selvedge-r2-bucket";

/// The header carrying the queue name on producer requests.
pub const QUEUE_HEADER: &str = "x-selvedge-queue";

/// The header marking asset reads on the binding socket.
pub const ASSETS_HEADER: &str = "x-selvedge-assets";
const ASSETS_KV_SERVICE: &str = "selvedge-assets-kv";
const ASSET_WORKER: &str = include_str!("vendor/asset-worker.js");
const ROUTER_WORKER: &str = include_str!("vendor/router-worker.js");

/// The header carrying `request.cf` as JSON on requests to a script's socket.
/// workerd removes it before tenant code runs. Without it, workerd would fill
/// `request.cf` with the socket peer's PID and UID.
pub const CF_HEADER: &str = "x-selvedge-cf";

/// The header marking trace events on the binding socket.
pub const TAIL_HEADER: &str = "x-selvedge-tail";
/// The tail worker every script with code reports to (`wrangler tail`).
pub const TAIL_SERVICE: &str = "selvedge-tail";
const TAIL_WORKER: &str = include_str!("tail.js");

/// The system worker that delivers Cron Triggers and queue batches.
pub const EVENTS_SERVICE: &str = "selvedge-events";
const EVENTS_WORKER: &str = include_str!("events.js");

/// The system worker behind D1 bindings and the D1 REST API.
pub const D1_SERVICE: &str = "selvedge-d1";
const D1_WORKER: &str = include_str!("d1.js");

/// The system worker behind Workflow bindings and the Workflows REST API.
pub const WORKFLOWS_SERVICE: &str = "selvedge-workflows";
const WORKFLOWS_WORKER: &str = include_str!("workflows.js");
const WORKFLOWS_ENGINE: &str = include_str!("vendor/workflows-engine.js");
const ZOD: &str = include_str!("vendor/zod.js");

/// The Workflows an account process runs: the registered ones and those that
/// bindings define, whose class is in a deployed script with code.
pub fn workflows(bundle: &AccountBundle) -> Vec<WorkflowDef> {
    let mut all: std::collections::BTreeMap<String, WorkflowDef> =
        bundle.workflows.iter().map(|w| (w.name.clone(), w.clone())).collect();
    for s in &bundle.scripts {
        for b in &s.bindings {
            if let Binding::Workflow { workflow_name, class_name, script_name, .. } = b {
                all.entry(workflow_name.clone()).or_insert_with(|| WorkflowDef {
                    name: workflow_name.clone(),
                    script_name: script_name.clone().unwrap_or_else(|| s.name.clone()),
                    class_name: class_name.clone(),
                });
            }
        }
    }
    all.into_values()
        .filter(|w| bundle.scripts.iter().any(|s| s.name == w.script_name && has_code(s)))
        .collect()
}

fn workflow_service(name: &str) -> String {
    format!("wf-{name}")
}

/// Whether a bundle needs the D1 system worker.
pub fn uses_d1(bundle: &AccountBundle) -> bool {
    !bundle.d1_databases.is_empty()
        || bundle.scripts.iter().any(|s| s.bindings.iter().any(|b| matches!(b, Binding::D1 { .. })))
}

/// Whether a bundle needs Durable Object storage (and, with a bucket, cluster mode).
pub fn uses_durable_objects(bundle: &AccountBundle) -> bool {
    uses_d1(bundle)
        || !workflows(bundle).is_empty()
        || bundle
            .scripts
            .iter()
            .any(|s| s.bindings.iter().any(|b| matches!(b, Binding::DurableObjectNamespace { .. })))
}

/// The system worker the agent probes to find a hung process.
pub const HEALTH_SERVICE: &str = "selvedge-health";
const HEALTH_WORKER: &str = "export default { fetch() { return new Response('ok'); } };";

const MISSING_SERVICE: &str = "selvedge-missing";
const MISSING_WORKER: &str = "export default { fetch() { return new Response(\
'Service binding target is not deployed', { status: 503 }); } };";

/// Workerd cluster membership (Durable Objects). See RFC 0001 §7.
#[derive(Clone, Debug)]
pub struct ClusterOptions {
    /// `s3://bucket/prefix`, unique to the account.
    pub bucket: String,
    /// `key=value` bucket options, e.g. `aws_endpoint=...`.
    pub bucket_options: Vec<String>,
    /// Where this instance listens for peers, `host:port`.
    pub address: String,
    /// Address peers use to reach this instance, if different from `address`.
    pub advertise: Option<String>,
    /// Hex of a secret of at least 32 bytes, unique to the account.
    pub key_hex: String,
    /// From SIGTERM to exit: the instance hands its Durable Objects to peers
    /// within three quarters of it, then deregisters. Killed sooner, it leaves
    /// them to wait for its lease to expire (20 seconds), unless the agent
    /// gives the lease up once the process has exited.
    pub stop_timeout_ms: u32,
}

#[derive(Clone, Debug)]
pub struct RenderOptions {
    /// Directory for the config and module files. Must not be shared between
    /// generations.
    pub dir: PathBuf,
    /// Directory for this generation's script sockets. Kept short: Unix socket
    /// paths are limited to 107 bytes.
    pub socket_dir: PathBuf,
    /// The agent's binding socket for this account.
    pub binding_socket: PathBuf,
    /// The newest compatibility date the workerd binary supports.
    pub max_compatibility_date: String,
    /// Directory for Durable Object storage (`localDisk`).
    pub do_storage_dir: PathBuf,
    pub cluster: Option<ClusterOptions>,
    /// Give Durable Object classes that have a container application their
    /// container. Without it, `ctx.container` is undefined.
    pub containers: Option<ContainerOptions>,
}

/// How an account process reaches its containers (RFC 0002).
#[derive(Clone, Debug)]
pub struct ContainerOptions {
    /// The agent's Docker API socket for this process.
    pub socket: PathBuf,
    /// The egress sidecar's image, as workerd names it to the agent.
    pub interceptor_image: String,
    /// Names this process's containers: workerd puts it in each one's name.
    pub session: String,
}

/// The account's Durable Object classes that have a container: workerd's
/// `uniqueKey` of each, and its definition.
pub fn container_classes(bundle: &AccountBundle) -> Vec<(String, &selvedge_core::ContainerDef)> {
    bundle
        .containers
        .iter()
        .map(|c| (durable_object_key(bundle.account_id.as_str(), &c.script_name, &c.class_name), c))
        .collect()
}

/// A rendered configuration and where each script listens.
#[derive(Clone, Debug)]
pub struct Rendered {
    pub config_path: PathBuf,
    /// `(script name, socket path)` in bundle order.
    pub sockets: Vec<(String, PathBuf)>,
    /// Whether the config needs `--experimental` (cluster mode).
    pub experimental: bool,
    /// Gradual deployments: `(script, share of its requests for the canary
    /// (0-100), the canary's version id)`. The canary's socket is
    /// `<script>@canary`.
    pub splits: Vec<(String, f64, String)>,
}

/// The socket name of a script's canary version.
pub fn canary_socket(script: &str) -> String {
    format!("{script}@canary")
}

/// Module bytes for rendering, looked up by SHA-256.
pub trait ModuleSource {
    fn module_bytes(&self, sha256: &str) -> Result<Vec<u8>>;
}

impl<F: Fn(&str) -> Result<Vec<u8>>> ModuleSource for F {
    fn module_bytes(&self, sha256: &str) -> Result<Vec<u8>> {
        self(sha256)
    }
}

/// Quotes a string as a Cap'n Proto text literal.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn worker_service(script: &str) -> String {
    format!("w-{script}")
}

fn asset_service(script: &str) -> String {
    format!("assets-{script}")
}

fn router_service(script: &str) -> String {
    format!("router-{script}")
}

/// Whether a script has code of its own (a Worker with only assets does not).
fn has_code(script: &BundleScript) -> bool {
    !script.modules.is_empty()
}

fn kv_service(namespace: &str) -> String {
    format!("kv-{namespace}")
}

fn r2_service(bucket: &str) -> String {
    format!("r2-{bucket}")
}

/// A Durable Object `uniqueKey`: stable per account, script and class, and one
/// path component as cluster mode requires.
pub fn durable_object_key(account: &str, script: &str, class: &str) -> String {
    let digest = selvedge_core::containers::do_namespace_digest(account, script, class);
    format!("do-{}", &digest[..40])
}

pub fn render(bundle: &AccountBundle, opts: &RenderOptions, modules: &dyn ModuleSource) -> Result<Rendered> {
    std::fs::create_dir_all(opts.dir.join("m"))
        .with_context(|| format!("creating {}", opts.dir.display()))?;
    // Service bindings reach a script's code directly, or its router when it
    // only has assets.
    let targets: std::collections::BTreeMap<&str, String> = bundle
        .scripts
        .iter()
        .map(|s| {
            let t = if has_code(s) { worker_service(&s.name) } else { router_service(&s.name) };
            (s.name.as_str(), t)
        })
        .collect();

    // Durable Object classes, keyed by (script, class), from every binding that
    // names one. A class is hosted by the script that defines it.
    let mut do_classes: BTreeSet<(String, String)> = BTreeSet::new();
    for script in &bundle.scripts {
        for b in &script.bindings {
            if let Binding::DurableObjectNamespace { class_name, script_name, .. } = b {
                let host = script_name.clone().unwrap_or_else(|| script.name.clone());
                if targets.get(host.as_str()).is_some_and(|t| t.starts_with("w-")) {
                    do_classes.insert((host, class_name.clone()));
                }
            }
        }
    }

    let mut services = Vec::new();
    let mut sockets_cfg = Vec::new();
    let mut sockets = Vec::new();
    let mut kv_namespaces = BTreeSet::new();
    let mut r2_buckets = BTreeSet::new();
    let mut queues = BTreeSet::new();
    let mut need_missing = false;

    let mut need_assets_kv = false;
    let mut splits = Vec::new();
    for (index, script) in bundle.scripts.iter().enumerate() {
        if let Some(assets) = &script.assets {
            need_assets_kv = true;
            let manifest = format!("m/{}", assets.manifest);
            let file = opts.dir.join(&manifest);
            if !file.exists() {
                std::fs::write(&file, modules.module_bytes(&assets.manifest)?)?;
            }
            let date = capped_date(&script.compatibility_date, &opts.max_compatibility_date);
            services.push(format!(
                "(name = {}, worker = (modules = [(name = \"asset-worker.js\", esModule = embed \"m/selvedge-asset-worker.js\")], \
                 compatibilityDate = \"2024-07-31\", compatibilityFlags = [\"nodejs_compat\", \"enable_ctx_exports\"], \
                 bindings = [(name = \"ASSETS_KV_NAMESPACE\", kvNamespace = {}), (name = \"ASSETS_MANIFEST\", data = embed {}), \
                 (name = \"CONFIG\", json = {})]))",
                quote(&asset_service(&script.name)),
                quote(ASSETS_KV_SERVICE),
                quote(&manifest),
                quote(&assets.asset_worker_config(&date, &script.compatibility_flags)),
            ));
            let user = if has_code(script) {
                worker_service(&script.name)
            } else {
                need_missing = true;
                MISSING_SERVICE.to_owned()
            };
            services.push(format!(
                "(name = {}, worker = (modules = [(name = \"router-worker.js\", esModule = embed \"m/selvedge-router-worker.js\")], \
                 compatibilityDate = \"2024-07-31\", \
                 compatibilityFlags = [\"nodejs_compat\", \"no_nodejs_compat_v2\", \"enable_ctx_exports\"], \
                 bindings = [(name = \"ASSET_WORKER\", service = {}), (name = \"USER_WORKER\", service = {}), \
                 (name = \"CONFIG\", json = {})]))",
                quote(&router_service(&script.name)),
                quote(&asset_service(&script.name)),
                quote(&user),
                quote(&assets.router_config(has_code(script))),
            ));
        } else if !has_code(script) {
            bail!("script `{}` has neither modules nor assets", script.name);
        }
        let entry =
            if script.assets.is_some() { router_service(&script.name) } else { worker_service(&script.name) };
        if !has_code(script) {
            let socket = opts.socket_dir.join(format!("{index}.sock"));
            sockets_cfg.push(format!(
                "(name = {}, address = {}, http = (forwardedProtoHeader = \"x-forwarded-proto\", cfBlobHeader = \"x-selvedge-cf\"), service = {})",
                quote(&script.name),
                quote(&format!("unix:{}", socket.display())),
                quote(&entry),
            ));
            // A reserved name (stored before names were checked) must not shadow
            // a system worker's socket.
            if !script.name.starts_with(selvedge_core::ids::RESERVED_SCRIPT_PREFIX) {
                sockets.push((script.name.clone(), socket));
            }
            continue;
        }
        let (worker, kvs, missing) = render_worker(bundle, script, opts, modules, &targets, &do_classes)?;
        need_missing |= missing;
        kv_namespaces.extend(kvs);
        r2_buckets.extend(script.bindings.iter().filter_map(|b| match b {
            Binding::R2Bucket { bucket_name, .. } => Some(bucket_name.clone()),
            _ => None,
        }));
        queues.extend(script.bindings.iter().filter_map(|b| match b {
            Binding::Queue { queue_name, .. } => Some(queue_name.clone()),
            _ => None,
        }));
        services.push(format!("(name = {}, worker = {worker})", quote(&worker_service(&script.name))));
        let socket = opts.socket_dir.join(format!("{index}.sock"));
        if socket.as_os_str().len() > 100 {
            bail!("socket path too long: {}", socket.display());
        }
        sockets_cfg.push(format!(
            "(name = {}, address = {}, http = (forwardedProtoHeader = \"x-forwarded-proto\", cfBlobHeader = \"x-selvedge-cf\"), service = {})",
            quote(&script.name),
            quote(&format!("unix:{}", socket.display())),
            quote(&entry),
        ));
        // A reserved name (stored before names were checked) must not shadow
        // a system worker's socket.
        if !script.name.starts_with(selvedge_core::ids::RESERVED_SCRIPT_PREFIX) {
            sockets.push((script.name.clone(), socket));
        }
        // A gradual deployment's other version: its own service and socket.
        // The API refuses splits of scripts with Durable Object classes or
        // assets, so it hosts neither.
        if let Some(c) = &script.canary {
            let none = BTreeSet::new();
            let (worker, kvs, missing) = render_worker(bundle, &c.script, opts, modules, &targets, &none)?;
            need_missing |= missing;
            kv_namespaces.extend(kvs);
            r2_buckets.extend(c.script.bindings.iter().filter_map(|b| match b {
                Binding::R2Bucket { bucket_name, .. } => Some(bucket_name.clone()),
                _ => None,
            }));
            queues.extend(c.script.bindings.iter().filter_map(|b| match b {
                Binding::Queue { queue_name, .. } => Some(queue_name.clone()),
                _ => None,
            }));
            let service = format!("wv-{}", script.name);
            services.push(format!("(name = {}, worker = {worker})", quote(&service)));
            let socket = opts.socket_dir.join(format!("{index}c.sock"));
            sockets_cfg.push(format!(
                "(name = {}, address = {}, http = (forwardedProtoHeader = \"x-forwarded-proto\", cfBlobHeader = \"x-selvedge-cf\"), service = {})",
                quote(&canary_socket(&script.name)),
                quote(&format!("unix:{}", socket.display())),
                quote(&service),
            ));
            sockets.push((canary_socket(&script.name), socket));
            splits.push((script.name.clone(), c.percentage, c.script.version_id.to_string()));
        }
    }
    if need_assets_kv {
        std::fs::write(opts.dir.join("m").join("selvedge-asset-worker.js"), ASSET_WORKER)?;
        std::fs::write(opts.dir.join("m").join("selvedge-router-worker.js"), ROUTER_WORKER)?;
        services.push(format!(
            "(name = {}, external = (address = {}, http = (injectRequestHeaders = [(name = {}, value = \"1\")])))",
            quote(ASSETS_KV_SERVICE),
            quote(&format!("unix:{}", opts.binding_socket.display())),
            quote(ASSETS_HEADER),
        ));
    }

    for ns in &kv_namespaces {
        services.push(format!(
            "(name = {}, external = (address = {}, http = (injectRequestHeaders = [(name = {}, value = {})])))",
            quote(&kv_service(ns)),
            quote(&format!("unix:{}", opts.binding_socket.display())),
            quote(KV_NAMESPACE_HEADER),
            quote(ns),
        ));
    }
    for queue in &queues {
        services.push(format!(
            "(name = {}, external = (address = {}, http = (injectRequestHeaders = [(name = {}, value = {})])))",
            quote(&format!("q-{queue}")),
            quote(&format!("unix:{}", opts.binding_socket.display())),
            quote(QUEUE_HEADER),
            quote(queue),
        ));
    }
    for bucket in &r2_buckets {
        services.push(format!(
            "(name = {}, external = (address = {}, http = (injectRequestHeaders = [(name = {}, value = {})])))",
            quote(&r2_service(bucket)),
            quote(&format!("unix:{}", opts.binding_socket.display())),
            quote(R2_BUCKET_HEADER),
            quote(bucket),
        ));
    }
    if need_missing {
        services.push(format!(
            "(name = {}, worker = (modules = [(name = \"missing.js\", esModule = {})], compatibilityDate = \"2024-01-01\"))",
            quote(MISSING_SERVICE),
            quote(MISSING_WORKER),
        ));
    }
    if !bundle.scripts.is_empty() {
        std::fs::write(opts.dir.join("m").join("selvedge-events.js"), EVENTS_WORKER)?;
        let bindings: Vec<String> = bundle
            .scripts
            .iter()
            .filter(|s| has_code(s))
            .map(|s| {
                format!(
                    "(name = {}, service = {})",
                    quote(&format!("S_{}", s.name)),
                    quote(&worker_service(&s.name))
                )
            })
            .collect();
        services.push(format!(
            "(name = {}, worker = (modules = [(name = \"events.js\", esModule = embed \"m/selvedge-events.js\")], \
             compatibilityDate = \"2025-09-01\", bindings = [{}]))",
            quote(EVENTS_SERVICE),
            bindings.join(", "),
        ));
        let socket = opts.socket_dir.join("events.sock");
        sockets_cfg.push(format!(
            "(name = {}, address = {}, http = (), service = {})",
            quote(EVENTS_SERVICE),
            quote(&format!("unix:{}", socket.display())),
            quote(EVENTS_SERVICE),
        ));
        sockets.push((EVENTS_SERVICE.to_owned(), socket));
    }
    if bundle.scripts.iter().any(has_code) {
        std::fs::write(opts.dir.join("m").join("selvedge-tail.js"), TAIL_WORKER)?;
        services.push(format!(
            "(name = {}, worker = (modules = [(name = \"tail.js\", esModule = embed \"m/selvedge-tail.js\")], \
             compatibilityDate = \"2025-09-01\", bindings = [(name = \"SINK\", service = \"selvedge-tail-sink\")]))",
            quote(TAIL_SERVICE),
        ));
        services.push(format!(
            "(name = \"selvedge-tail-sink\", external = (address = {}, http = (injectRequestHeaders = \
             [(name = {}, value = \"1\")])))",
            quote(&format!("unix:{}", opts.binding_socket.display())),
            quote(TAIL_HEADER),
        ));
    }
    let wfs = workflows(bundle);
    if !wfs.is_empty() {
        std::fs::write(opts.dir.join("m").join("selvedge-workflows.js"), WORKFLOWS_WORKER)?;
        std::fs::write(opts.dir.join("m").join("selvedge-workflows-engine.js"), WORKFLOWS_ENGINE)?;
        std::fs::write(opts.dir.join("m").join("selvedge-zod.js"), ZOD)?;
        let mut front = vec!["(name = \"INDEX\", durableObjectNamespace = \"WorkflowIndex\")".to_owned()];
        for w in &wfs {
            let service = workflow_service(&w.name);
            let name = serde_json::json!(w.name).to_string();
            services.push(format!(
                "(name = {}, worker = (modules = [(name = \"workflows.mjs\", esModule = embed \"m/selvedge-workflows-engine.js\")], \
                 compatibilityDate = \"2024-10-22\", \
                 durableObjectNamespaces = [(className = \"Engine\", uniqueKey = {}, enableSql = true, preventEviction = true)], \
                 durableObjectStorage = (localDisk = \"do-storage\"), \
                 bindings = [(name = \"ENGINE\", durableObjectNamespace = \"Engine\"), \
                 (name = \"USER_WORKFLOW\", service = (name = {}, entrypoint = {})), \
                 (name = \"BINDING_NAME\", json = {}), (name = \"WORKFLOW_NAME\", json = {})]))",
                quote(&service),
                quote(&durable_object_key(bundle.account_id.as_str(), &service, "Engine")),
                quote(&worker_service(&w.script_name)),
                quote(&w.class_name),
                quote(&name),
                quote(&name),
            ));
            front.push(format!(
                "(name = {}, service = (name = {}, entrypoint = \"WorkflowBinding\"))",
                quote(&format!("B_{}", w.name)),
                quote(&service)
            ));
            front.push(format!(
                "(name = {}, durableObjectNamespace = (className = \"Engine\", serviceName = {}))",
                quote(&format!("E_{}", w.name)),
                quote(&service)
            ));
        }
        services.push(format!(
            "(name = {}, worker = (modules = [(name = \"workflows.js\", esModule = embed \"m/selvedge-workflows.js\")], \
             compatibilityDate = \"2025-09-01\", \
             durableObjectNamespaces = [(className = \"WorkflowIndex\", uniqueKey = {}, enableSql = true)], \
             durableObjectStorage = (localDisk = \"do-storage\"), bindings = [{}]))",
            quote(WORKFLOWS_SERVICE),
            quote(&durable_object_key(bundle.account_id.as_str(), WORKFLOWS_SERVICE, "WorkflowIndex")),
            front.join(", "),
        ));
        let socket = opts.socket_dir.join("workflows.sock");
        sockets_cfg.push(format!(
            "(name = {}, address = {}, http = (), service = {})",
            quote(WORKFLOWS_SERVICE),
            quote(&format!("unix:{}", socket.display())),
            quote(WORKFLOWS_SERVICE),
        ));
        sockets.push((WORKFLOWS_SERVICE.to_owned(), socket));
    }
    let d1 = uses_d1(bundle);
    if d1 {
        std::fs::write(opts.dir.join("m").join("selvedge-d1.js"), D1_WORKER)?;
        services.push(format!(
            "(name = {}, worker = (modules = [(name = \"d1.js\", esModule = embed \"m/selvedge-d1.js\")], \
             compatibilityDate = \"2025-09-01\", bindings = [(name = \"D1\", durableObjectNamespace = \"D1Database\")], \
             durableObjectNamespaces = [(className = \"D1Database\", uniqueKey = {}, enableSql = true)], \
             durableObjectStorage = (localDisk = \"do-storage\")))",
            quote(D1_SERVICE),
            quote(&durable_object_key(bundle.account_id.as_str(), D1_SERVICE, "D1Database")),
        ));
        let socket = opts.socket_dir.join("d1.sock");
        sockets_cfg.push(format!(
            "(name = {}, address = {}, http = (), service = {})",
            quote(D1_SERVICE),
            quote(&format!("unix:{}", socket.display())),
            quote(D1_SERVICE),
        ));
        sockets.push((D1_SERVICE.to_owned(), socket));
    }
    if !do_classes.is_empty() || d1 || !wfs.is_empty() {
        std::fs::create_dir_all(&opts.do_storage_dir)?;
        services.push(format!(
            "(name = \"do-storage\", disk = (path = {}, writable = true))",
            quote(&opts.do_storage_dir.display().to_string()),
        ));
    }
    services.push(format!(
        "(name = {}, worker = (modules = [(name = \"health.js\", esModule = {})], compatibilityDate = \"2024-01-01\"))",
        quote(HEALTH_SERVICE),
        quote(HEALTH_WORKER),
    ));
    let socket = opts.socket_dir.join("health.sock");
    sockets_cfg.push(format!(
        "(name = {}, address = {}, http = (), service = {})",
        quote(HEALTH_SERVICE),
        quote(&format!("unix:{}", socket.display())),
        quote(HEALTH_SERVICE),
    ));
    sockets.push((HEALTH_SERVICE.to_owned(), socket));
    services.push(
        "(name = \"internet\", network = (allow = [\"public\"], deny = [\"private\", \"local\"], \
         tlsOptions = (trustBrowserCas = true)))"
            .to_owned(),
    );

    let mut text = String::new();
    text.push_str("# Generated by Selvedge. Do not edit.\n");
    text.push_str("using Workerd = import \"/workerd/workerd.capnp\";\n\n");
    text.push_str("const config :Workerd.Config = (\n  services = [\n");
    for s in &services {
        let _ = writeln!(text, "    {s},");
    }
    text.push_str("  ],\n  sockets = [\n");
    for s in &sockets_cfg {
        let _ = writeln!(text, "    {s},");
    }
    text.push_str("  ],\n");
    if !wfs.is_empty() {
        // The engine imports zod by miniflare's module name.
        text.push_str(
            "  extensions = [(modules = [(name = \"miniflare:zod\", esModule = embed \"m/selvedge-zod.js\")])],\n",
        );
    }
    let experimental = opts.cluster.is_some() && (!do_classes.is_empty() || d1 || !wfs.is_empty());
    if experimental {
        let c = opts.cluster.as_ref().expect("checked above");
        let mut fields = vec![
            format!("bucket = {}", quote(&c.bucket)),
            format!(
                "bucketOptions = [{}]",
                c.bucket_options.iter().map(|o| quote(o)).collect::<Vec<_>>().join(", ")
            ),
            format!("address = {}", quote(&c.address)),
            format!("key = {}", quote(&c.key_hex)),
            format!("stopTimeoutMs = {}", c.stop_timeout_ms),
        ];
        if let Some(adv) = &c.advertise {
            fields.push(format!("advertise = {}", quote(adv)));
        }
        let _ = writeln!(text, "  cluster = ({}),", fields.join(", "));
    }
    text.push_str(");\n");

    let config_path = opts.dir.join("config.capnp");
    std::fs::write(&config_path, text)?;
    Ok(Rendered { config_path, sockets, experimental, splits })
}

/// Renders one script's `Worker` struct. Returns it, the KV namespaces it binds,
/// and whether it needs the missing-service stub.
fn render_worker(
    bundle: &AccountBundle,
    script: &BundleScript,
    opts: &RenderOptions,
    modules: &dyn ModuleSource,
    targets: &std::collections::BTreeMap<&str, String>,
    do_classes: &BTreeSet<(String, String)>,
) -> Result<(String, Vec<String>, bool)> {
    let main =
        script.modules.iter().position(|m| m.name == script.main_module).with_context(|| {
            format!("script `{}` has no main module `{}`", script.name, script.main_module)
        })?;
    // workerd treats the first module as the entry point.
    let mut ordered: Vec<_> = script.modules.iter().collect();
    ordered.swap(0, main);

    let mut module_cfg = Vec::new();
    for m in ordered {
        let bytes = modules.module_bytes(&m.sha256)?;
        let file = opts.dir.join("m").join(&m.sha256);
        if !file.exists() {
            std::fs::write(&file, &bytes)?;
        }
        let field = match m.kind {
            ModuleKind::EsModule => "esModule",
            ModuleKind::CommonJs => "commonJsModule",
            ModuleKind::Text => "text",
            ModuleKind::Data => "data",
            ModuleKind::Wasm => "wasm",
            ModuleKind::Json => "json",
            ModuleKind::Python => "pythonModule",
        };
        module_cfg.push(format!(
            "(name = {}, {field} = embed {})",
            quote(&m.name),
            quote(&format!("m/{}", m.sha256))
        ));
    }

    let mut bindings = Vec::new();
    let mut kvs = Vec::new();
    let mut missing = false;
    for b in &script.bindings {
        let value = match b {
            Binding::PlainText { text, .. } => format!("text = {}", quote(text)),
            Binding::SecretText { text, .. } => {
                format!("text = {}", quote(text.as_deref().unwrap_or_default()))
            }
            Binding::Json { json, .. } => format!("json = {}", quote(&json.to_string())),
            Binding::KvNamespace { namespace_id, .. } => {
                kvs.push(namespace_id.to_string());
                format!("kvNamespace = {}", quote(&kv_service(namespace_id.as_str())))
            }
            Binding::Service { service, entrypoint, .. } => {
                let target = match targets.get(service.as_str()) {
                    // Named entrypoints exist only in a script's code.
                    Some(t) if entrypoint.is_none() || t.starts_with("w-") => t.clone(),
                    _ => {
                        missing = true;
                        MISSING_SERVICE.to_owned()
                    }
                };
                match entrypoint {
                    Some(ep) if target != MISSING_SERVICE => {
                        format!("service = (name = {}, entrypoint = {})", quote(&target), quote(ep))
                    }
                    _ => format!("service = {}", quote(&target)),
                }
            }
            Binding::Queue { queue_name, .. } => format!("queue = {}", quote(&format!("q-{queue_name}"))),
            Binding::D1 { id, .. } => format!(
                "wrapped = (moduleName = \"cloudflare-internal:d1-api\", innerBindings = [(name = \"fetcher\", \
                 service = (name = {}, props = (json = {})))])",
                quote(D1_SERVICE),
                quote(&serde_json::json!({ "db": id }).to_string()),
            ),
            Binding::R2Bucket { bucket_name, .. } => {
                format!("r2Bucket = {}", quote(&r2_service(bucket_name)))
            }
            Binding::Workflow { workflow_name, .. } => {
                let defined = workflows(bundle).iter().any(|w| w.name == *workflow_name);
                if !defined {
                    missing = true;
                }
                let fetcher = if defined {
                    format!(
                        "(name = {}, entrypoint = \"Binding\", props = (json = {}))",
                        quote(WORKFLOWS_SERVICE),
                        quote(&serde_json::json!({ "workflow": workflow_name }).to_string())
                    )
                } else {
                    quote(MISSING_SERVICE)
                };
                format!(
                    "wrapped = (moduleName = \"cloudflare-internal:workflows-api\", innerBindings = \
                     [(name = \"fetcher\", service = {fetcher})])"
                )
            }
            Binding::Assets { .. } => {
                if script.assets.is_some() {
                    format!("service = {}", quote(&asset_service(&script.name)))
                } else {
                    missing = true;
                    format!("service = {}", quote(MISSING_SERVICE))
                }
            }
            Binding::DurableObjectNamespace { class_name, script_name, .. } => {
                let host = script_name.clone().unwrap_or_else(|| script.name.clone());
                if !targets.get(host.as_str()).is_some_and(|t| t.starts_with("w-")) {
                    bail!(
                        "script `{}` binds Durable Object class `{class_name}` of script `{host}`, which is not deployed",
                        script.name
                    );
                }
                format!(
                    "durableObjectNamespace = (className = {}, serviceName = {})",
                    quote(class_name),
                    quote(&worker_service(&host))
                )
            }
        };
        bindings.push(format!("(name = {}, {value})", quote(b.name())));
    }

    let date = capped_date(&script.compatibility_date, &opts.max_compatibility_date);

    let mut fields = vec![
        format!("modules = [{}]", module_cfg.join(", ")),
        format!("compatibilityDate = {}", quote(&date)),
        // Experimental flags are refused on upload; versions stored before that
        // lose them here.
        format!(
            "compatibilityFlags = [{}]",
            script
                .compatibility_flags
                .iter()
                .filter(|f| !crate::is_experimental_flag(f))
                .map(|f| quote(f))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        format!("bindings = [{}]", bindings.join(", ")),
        "globalOutbound = \"internet\"".to_owned(),
        format!(
            "tails = [(name = {}, props = (json = {}))]",
            quote(TAIL_SERVICE),
            quote(&serde_json::json!({ "script": script.name }).to_string())
        ),
    ];
    let hosted: Vec<_> = do_classes.iter().filter(|(s, _)| *s == script.name).collect();
    if !hosted.is_empty() {
        let image_of = |class: &str| {
            opts.containers.as_ref()?;
            bundle.containers.iter().find(|c| c.script_name == script.name && c.class_name == class)
        };
        let ns: Vec<_> = hosted
            .iter()
            .map(|(_, class)| {
                let container = image_of(class)
                    .map(|c| format!(", container = (imageName = {})", quote(&c.image)))
                    .unwrap_or_default();
                format!(
                    "(className = {}, uniqueKey = {}, enableSql = true{container})",
                    quote(class),
                    quote(&durable_object_key(bundle.account_id.as_str(), &script.name, class))
                )
            })
            .collect();
        fields.push(format!("durableObjectNamespaces = [{}]", ns.join(", ")));
        if let Some(c) =
            opts.containers.as_ref().filter(|_| hosted.iter().any(|(_, class)| image_of(class).is_some()))
        {
            // Not a Docker daemon: the agent's socket, which runs only the
            // account's own applications (RFC 0002).
            fields.push(format!(
                "containerEngine = (localDocker = (socketPath = {}, containerEgressInterceptorImage = {}, sessionId = {}))",
                quote(&format!("unix:{}", c.socket.display())),
                quote(&c.interceptor_image),
                quote(&c.session)
            ));
        }
        fields.push("durableObjectStorage = (localDisk = \"do-storage\")".to_owned());
    }
    Ok((format!("({})", fields.join(", ")), kvs, missing))
}

/// A compatibility date lowered to the newest the workerd binary supports.
fn capped_date(date: &str, max: &str) -> String {
    if date > max { max.to_owned() } else { date.to_owned() }
}

/// Reads the compatibility date a workerd binary supports from `workerd --version`
/// (`workerd 2026-11-06`).
pub async fn workerd_max_date(workerd: &Path) -> Result<String> {
    let out = tokio::process::Command::new(workerd).arg("--version").output().await?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .find(|w| w.len() == 10 && w.as_bytes()[4] == b'-' && w.as_bytes()[7] == b'-')
        .map(str::to_owned)
        .with_context(|| format!("unexpected `workerd --version` output: {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use selvedge_core::{HexId, Module};

    #[test]
    fn experimental_flags() {
        assert!(crate::is_experimental_flag("unsafe_module"));
        assert!(crate::is_experimental_flag("experimental"));
        assert!(!crate::is_experimental_flag("nodejs_compat"));
        assert!(!crate::is_experimental_flag("enable_ctx_exports"));
        assert!(!crate::is_experimental_flag("# workerd"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("a\"b\\c\nd\u{1}é"), "\"a\\\"b\\\\c\\nd\\x01é\"");
    }

    #[test]
    fn renders_scripts_bindings_and_missing_services() {
        let dir = tempfile::tempdir().unwrap();
        let ns: HexId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        let bundle = AccountBundle {
            account_id: HexId::random(),
            generation: 1,
            scripts: vec![BundleScript {
                name: "hello".into(),
                version_id: selvedge_core::new_version_id(),
                main_module: "index.js".into(),
                compatibility_date: "2999-01-01".into(),
                compatibility_flags: vec!["nodejs_compat".into()],
                modules: vec![
                    Module { name: "util.js".into(), kind: ModuleKind::EsModule, sha256: "aa".into() },
                    Module { name: "index.js".into(), kind: ModuleKind::EsModule, sha256: "bb".into() },
                ],
                bindings: vec![
                    Binding::PlainText { name: "GREETING".into(), text: "hi \"there\"".into() },
                    Binding::KvNamespace { name: "KV".into(), namespace_id: ns.clone() },
                    Binding::Service { name: "OTHER".into(), service: "nope".into(), entrypoint: None },
                ],
                assets: None,
                canary: None,
            }],
            d1_databases: vec![],
            workflows: vec![],
            containers: vec![],
        };
        let opts = RenderOptions {
            dir: dir.path().join("g1"),
            socket_dir: dir.path().join("s"),
            binding_socket: dir.path().join("b.sock"),
            max_compatibility_date: "2026-11-06".into(),
            do_storage_dir: dir.path().join("do"),
            cluster: None,
            containers: None,
        };
        let src = |sha: &str| Ok(format!("// {sha}").into_bytes());
        let r = render(&bundle, &opts, &src).unwrap();
        let text = std::fs::read_to_string(&r.config_path).unwrap();
        // The main module comes first.
        assert!(text.find("m/bb").unwrap() < text.find("m/aa").unwrap());
        assert!(text.contains("compatibilityDate = \"2026-11-06\""));
        assert!(text.contains(&format!("kvNamespace = \"kv-{ns}\"")));
        assert!(text.contains(&format!("value = \"{ns}\"")));
        assert!(text.contains("service = \"selvedge-missing\""));
        assert!(text.contains("hi \\\"there\\\""));
        assert!(!r.experimental);
        // The script and the events and health system workers.
        assert_eq!(
            r.sockets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["hello", EVENTS_SERVICE, HEALTH_SERVICE]
        );
    }

    #[test]
    fn renders_cluster_membership_with_its_stop_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = AccountBundle {
            account_id: HexId::random(),
            generation: 1,
            scripts: vec![BundleScript {
                name: "rooms".into(),
                version_id: selvedge_core::new_version_id(),
                main_module: "index.js".into(),
                compatibility_date: "2026-01-01".into(),
                compatibility_flags: vec![],
                modules: vec![Module {
                    name: "index.js".into(),
                    kind: ModuleKind::EsModule,
                    sha256: "aa".into(),
                }],
                bindings: vec![Binding::DurableObjectNamespace {
                    name: "ROOM".into(),
                    class_name: "Room".into(),
                    script_name: None,
                }],
                assets: None,
                canary: None,
            }],
            d1_databases: vec![],
            workflows: vec![],
            containers: vec![],
        };
        let opts = RenderOptions {
            dir: dir.path().join("g1"),
            socket_dir: dir.path().join("s"),
            binding_socket: dir.path().join("b.sock"),
            max_compatibility_date: "2026-11-06".into(),
            do_storage_dir: dir.path().join("do"),
            cluster: Some(ClusterOptions {
                bucket: "s3://b/do/acct".into(),
                bucket_options: vec!["aws_endpoint=http://127.0.0.1:9".into()],
                address: "10.0.0.1:0".into(),
                advertise: None,
                key_hex: "ab".repeat(32),
                stop_timeout_ms: 8000,
            }),
            containers: None,
        };
        let src = |sha: &str| Ok(format!("// {sha}").into_bytes());
        let r = render(&bundle, &opts, &src).unwrap();
        let text = std::fs::read_to_string(&r.config_path).unwrap();
        assert!(r.experimental);
        assert!(text.contains("cluster = (bucket = \"s3://b/do/acct\""), "{text}");
        assert!(text.contains("stopTimeoutMs = 8000"), "{text}");
    }
    #[test]
    fn containers() {
        use selvedge_core::ContainerDef;
        let dir = tempfile::tempdir().unwrap();
        let account: selvedge_core::AccountId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        let script = |name: &str, class: &str| BundleScript {
            name: name.into(),
            version_id: selvedge_core::new_version_id(),
            main_module: "worker.js".into(),
            compatibility_date: "2026-10-01".into(),
            compatibility_flags: vec![],
            modules: vec![Module {
                name: "worker.js".into(),
                kind: ModuleKind::EsModule,
                sha256: "aa".into(),
            }],
            bindings: vec![Binding::DurableObjectNamespace {
                name: "DO".into(),
                class_name: class.into(),
                script_name: None,
            }],
            assets: None,
            canary: None,
        };
        let bundle = AccountBundle {
            account_id: account.clone(),
            generation: 1,
            scripts: vec![script("boxes", "Box"), script("plain", "Room")],
            d1_databases: vec![],
            workflows: vec![],
            containers: vec![ContainerDef {
                script_name: "boxes".into(),
                class_name: "Box".into(),
                image: "registry.test/acct/app@sha256:00".into(),
            }],
        };
        let mut opts = RenderOptions {
            dir: dir.path().join("g1"),
            socket_dir: dir.path().join("s"),
            binding_socket: dir.path().join("b.sock"),
            max_compatibility_date: "2026-11-06".into(),
            do_storage_dir: dir.path().join("do"),
            cluster: None,
            containers: Some(ContainerOptions {
                socket: "/run/s/d.sock".into(),
                interceptor_image: "selvedge/container-egress".into(),
                session: "ab12-0123456789ab-g1-1".into(),
            }),
        };
        let src = |sha: &str| Ok(format!("// {sha}").into_bytes());
        let text = std::fs::read_to_string(render(&bundle, &opts, &src).unwrap().config_path).unwrap();
        // The class with an application gets its container; its script, the engine.
        assert!(text.contains("container = (imageName = \"registry.test/acct/app@sha256:00\")"), "{text}");
        assert_eq!(
            text.matches("containerEngine = (localDocker = (socketPath = \"unix:/run/s/d.sock\"").count(),
            1
        );
        assert!(text.contains("sessionId = \"ab12-0123456789ab-g1-1\""));
        assert_eq!(text.matches("container = (").count(), 1);
        let classes = container_classes(&bundle);
        assert_eq!(classes[0].0, durable_object_key(account.as_str(), "boxes", "Box"));

        // An agent that does not run containers renders none.
        opts.dir = dir.path().join("g2");
        opts.containers = None;
        let text = std::fs::read_to_string(render(&bundle, &opts, &src).unwrap().config_path).unwrap();
        assert!(!text.contains("containerEngine") && !text.contains("imageName"));
    }
}
