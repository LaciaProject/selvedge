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
//!   a KV-protocol service on the agent's binding socket.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use selvedge_core::{AccountBundle, Binding, BundleScript, ModuleKind};

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

/// Whether a bundle needs the D1 system worker.
pub fn uses_d1(bundle: &AccountBundle) -> bool {
    !bundle.d1_databases.is_empty()
        || bundle.scripts.iter().any(|s| s.bindings.iter().any(|b| matches!(b, Binding::D1 { .. })))
}

/// Whether a bundle needs Durable Object storage (and, with a bucket, cluster mode).
pub fn uses_durable_objects(bundle: &AccountBundle) -> bool {
    uses_d1(bundle)
        || bundle
            .scripts
            .iter()
            .any(|s| s.bindings.iter().any(|b| matches!(b, Binding::DurableObjectNamespace { .. })))
}

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
}

/// A rendered configuration and where each script listens.
#[derive(Clone, Debug)]
pub struct Rendered {
    pub config_path: PathBuf,
    /// `(script name, socket path)` in bundle order.
    pub sockets: Vec<(String, PathBuf)>,
    /// Whether the config needs `--experimental` (cluster mode).
    pub experimental: bool,
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
    let digest = selvedge_core::sha256_hex(format!("{account}\0{script}\0{class}").as_bytes());
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
            sockets.push((script.name.clone(), socket));
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
        sockets.push((script.name.clone(), socket));
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
    if !do_classes.is_empty() || d1 {
        std::fs::create_dir_all(&opts.do_storage_dir)?;
        services.push(format!(
            "(name = \"do-storage\", disk = (path = {}, writable = true))",
            quote(&opts.do_storage_dir.display().to_string()),
        ));
    }
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
    let experimental = opts.cluster.is_some() && (!do_classes.is_empty() || d1);
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
        ];
        if let Some(adv) = &c.advertise {
            fields.push(format!("advertise = {}", quote(adv)));
        }
        let _ = writeln!(text, "  cluster = ({}),", fields.join(", "));
    }
    text.push_str(");\n");

    let config_path = opts.dir.join("config.capnp");
    std::fs::write(&config_path, text)?;
    Ok(Rendered { config_path, sockets, experimental })
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
        let ns: Vec<_> = hosted
            .iter()
            .map(|(_, class)| {
                format!(
                    "(className = {}, uniqueKey = {}, enableSql = true)",
                    quote(class),
                    quote(&durable_object_key(bundle.account_id.as_str(), &script.name, class))
                )
            })
            .collect();
        fields.push(format!("durableObjectNamespaces = [{}]", ns.join(", ")));
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
            }],
            d1_databases: vec![],
        };
        let opts = RenderOptions {
            dir: dir.path().join("g1"),
            socket_dir: dir.path().join("s"),
            binding_socket: dir.path().join("b.sock"),
            max_compatibility_date: "2026-11-06".into(),
            do_storage_dir: dir.path().join("do"),
            cluster: None,
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
        // The script and the events system worker.
        assert_eq!(r.sockets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["hello", EVENTS_SERVICE]);
    }
}
