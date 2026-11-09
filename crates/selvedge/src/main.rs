//! `selvedge`: one binary for every Selvedge role.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use selvedge_server::agent::{AgentCluster, AgentConfig};
use selvedge_server::api::ApiState;
use selvedge_server::gateway::{GatewayConfig, Runners};
use selvedge_store::{Blobs, Db, SecretBox};

#[derive(Parser)]
#[command(
    name = "selvedge",
    version,
    about = "A highly available, Workers-compatible platform for Kubernetes"
)]
struct Cli {
    /// Log as JSON lines.
    #[arg(long, env = "SELVEDGE_LOG_JSON", global = true)]
    log_json: bool,
    /// Serve Prometheus metrics (`GET /metrics`, plain HTTP) on this address.
    #[arg(long, env = "SELVEDGE_METRICS_LISTEN", global = true)]
    metrics_listen: Option<SocketAddr>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the /client/v4 API and internal endpoints.
    Api(ApiArgs),
    /// Serve the binding data service (KV).
    Data(DataArgs),
    /// Run workers: one workerd process per account.
    Agent(AgentArgs),
    /// Route requests by hostname to runners.
    Gateway(GatewayArgs),
    /// Run every role in one process (the minimal profile).
    All(Box<AllArgs>),
    /// Apply database migrations and exit.
    Migrate(DbArgs),
    /// Administrative commands.
    #[command(subcommand)]
    Admin(AdminCommand),
}

#[derive(Args, Clone)]
struct DbArgs {
    /// PostgreSQL URL.
    #[arg(long, env = "SELVEDGE_DATABASE_URL")]
    database_url: String,
    #[arg(long, env = "SELVEDGE_DATABASE_POOL", default_value_t = 16)]
    database_pool: usize,
}

#[derive(Args, Clone)]
struct InternalArgs {
    /// Shared secret between platform components.
    #[arg(long, env = "SELVEDGE_INTERNAL_TOKEN", hide_env_values = true)]
    internal_token: String,
}

#[derive(Args, Clone)]
struct ApiArgs {
    #[command(flatten)]
    db: DbArgs,
    #[command(flatten)]
    internal: InternalArgs,
    #[arg(long, env = "SELVEDGE_API_LISTEN", default_value = "0.0.0.0:8787")]
    listen: SocketAddr,
    /// The internal endpoints for agents and gateways, with mTLS. With mTLS
    /// on, the public listener does not serve them.
    #[arg(long, env = "SELVEDGE_API_INTERNAL_LISTEN")]
    internal_listen: Option<SocketAddr>,
    /// Where module bytes live: s3://bucket/prefix, file:///dir or memory://.
    #[arg(long, env = "SELVEDGE_BLOB_URL")]
    blob_url: String,
    /// 64 hex characters; encrypts secrets at rest.
    #[arg(long, env = "SELVEDGE_MASTER_KEY", hide_env_values = true)]
    master_key: String,
    /// A platform admin token to create if missing.
    #[arg(long, env = "SELVEDGE_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    /// Zone for <script>.<subdomain>.<zone> hostnames.
    #[arg(long, env = "SELVEDGE_WORKERS_DEV_ZONE")]
    workers_dev_zone: Option<String>,
    /// Zone for public bucket URLs, pub-<id>.<zone> (like r2.dev); the data
    /// service's R2 S3 listener serves them.
    #[arg(long, env = "SELVEDGE_R2_DEV_ZONE")]
    r2_dev_zone: Option<String>,
    /// The gateway, for API requests into account processes (D1 queries).
    #[arg(long, env = "SELVEDGE_GATEWAY_URL")]
    gateway_url: Option<String>,
}

#[derive(Args, Clone)]
struct DataArgs {
    #[command(flatten)]
    db: DbArgs,
    #[command(flatten)]
    internal: InternalArgs,
    #[arg(long, env = "SELVEDGE_DATA_LISTEN", default_value = "0.0.0.0:8788")]
    listen: SocketAddr,
    /// Where R2 object bytes live (the same store as the API's blobs).
    #[arg(long, env = "SELVEDGE_BLOB_URL")]
    blob_url: String,
    /// The gateway, for delivering Cron Triggers.
    #[arg(long, env = "SELVEDGE_GATEWAY_URL")]
    gateway_url: Option<String>,
    /// Serve R2's S3-compatible API here (path-style), e.g. 0.0.0.0:8790.
    /// Public buckets are served here too.
    #[arg(long, env = "SELVEDGE_R2_S3_LISTEN")]
    r2_s3_listen: Option<SocketAddr>,
    /// Zone for public bucket URLs, pub-<id>.<zone>, as the API names them.
    #[arg(long, env = "SELVEDGE_R2_DEV_ZONE")]
    r2_dev_zone: Option<String>,
    /// The R2 S3 endpoint's domain, for virtual-hosted requests
    /// (<bucket>.<domain>); path-style requests work either way.
    #[arg(long, env = "SELVEDGE_R2_S3_DOMAIN")]
    r2_s3_domain: Option<String>,
}

#[derive(Args, Clone)]
struct AgentArgs {
    #[command(flatten)]
    internal: InternalArgs,
    #[arg(long, env = "SELVEDGE_AGENT_LISTEN", default_value = "0.0.0.0:8789")]
    listen: SocketAddr,
    #[arg(long, env = "SELVEDGE_API_URL", default_value = "http://127.0.0.1:8787")]
    api_url: String,
    /// The data service, host:port.
    #[arg(long, env = "SELVEDGE_DATA_ADDR", default_value = "127.0.0.1:8788")]
    data_addr: String,
    /// The official workerd binary.
    #[arg(long, env = "SELVEDGE_WORKERD", default_value = "workerd")]
    workerd: PathBuf,
    #[arg(long, env = "SELVEDGE_STATE_DIR", default_value = "/var/lib/selvedge")]
    state_dir: PathBuf,
    /// Keep short: Unix socket paths must fit in 107 bytes.
    #[arg(long, env = "SELVEDGE_RUN_DIR", default_value = "/run/selvedge")]
    run_dir: PathBuf,
    /// Stop an account's process after this many idle seconds.
    #[arg(long, env = "SELVEDGE_IDLE_TIMEOUT", default_value_t = 600)]
    idle_timeout: u64,
    /// Bucket for Durable Objects (workerd cluster mode): s3://bucket/prefix.
    #[arg(long, env = "SELVEDGE_DO_BUCKET")]
    do_bucket: Option<String>,
    /// key=value bucket options (aws_endpoint=..., aws_region=...). Repeatable.
    #[arg(long = "do-bucket-option", env = "SELVEDGE_DO_BUCKET_OPTIONS", value_delimiter = ',')]
    do_bucket_options: Vec<String>,
    /// IP workerd binds its cluster port on; peers must reach it (the pod IP).
    #[arg(long, env = "SELVEDGE_CLUSTER_HOST", default_value = "127.0.0.1")]
    cluster_host: String,
    /// Secret shared by all agents; per-account cluster keys derive from it.
    #[arg(long, env = "SELVEDGE_CLUSTER_SECRET", hide_env_values = true)]
    cluster_secret: Option<String>,
    /// Allow Durable Objects without a bucket. Only safe with a single runner.
    #[arg(long, env = "SELVEDGE_ALLOW_LOCAL_DURABLE_OBJECTS")]
    allow_local_durable_objects: bool,
    /// Seconds to keep serving, unhealthy, after SIGTERM before draining.
    #[arg(long, env = "SELVEDGE_DRAIN_DELAY", default_value_t = 5)]
    drain_delay: u64,
    /// Run each account's workerd as its own Linux user, with UIDs from this
    /// range (`first-last`, e.g. 10000-59999). Needs root with CAP_SETUID,
    /// CAP_SETGID, CAP_CHOWN, CAP_DAC_OVERRIDE, CAP_FOWNER, CAP_KILL and CAP_SYS_NICE.
    #[arg(long, env = "SELVEDGE_ACCOUNT_UIDS")]
    account_uids: Option<String>,
    /// Memory limit per account process, in MiB (0: no CPU, memory or
    /// process limits).
    #[arg(long, env = "SELVEDGE_ACCOUNT_MEMORY_MB", default_value_t = 1024)]
    account_memory_mb: u64,
    /// CPUs an account process may use.
    #[arg(long, env = "SELVEDGE_ACCOUNT_CPUS", default_value_t = 1.0)]
    account_cpus: f64,
    /// Processes and threads per account process.
    #[arg(long, env = "SELVEDGE_ACCOUNT_PIDS", default_value_t = 1024)]
    account_pids: u64,
    /// Local disk for cached static assets, in MiB.
    #[arg(long, env = "SELVEDGE_ASSET_CACHE_MB", default_value_t = 1024)]
    asset_cache_mb: u64,
    /// Kill an account process that has not answered a health probe for this
    /// many seconds, e.g. one stuck in an endless loop (0: never).
    #[arg(long, env = "SELVEDGE_HANG_TIMEOUT", default_value_t = 30)]
    hang_timeout: u64,
}

#[derive(Args, Clone)]
struct GatewayArgs {
    #[command(flatten)]
    internal: InternalArgs,
    #[arg(long, env = "SELVEDGE_GATEWAY_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// For the API and data service's requests into account processes, with
    /// mTLS. With mTLS on, the public listener does not route those.
    #[arg(long, env = "SELVEDGE_GATEWAY_INTERNAL_LISTEN")]
    internal_listen: Option<SocketAddr>,
    #[arg(long, env = "SELVEDGE_API_URL", default_value = "http://127.0.0.1:8787")]
    api_url: String,
    /// Runner agents, host:port, comma separated.
    #[arg(long, env = "SELVEDGE_RUNNERS", value_delimiter = ',')]
    runners: Vec<String>,
    /// A DNS name resolving to every runner (headless Service), with --runner-port.
    #[arg(long, env = "SELVEDGE_RUNNER_DNS")]
    runner_dns: Option<String>,
    #[arg(long, env = "SELVEDGE_RUNNER_PORT", default_value_t = 8789)]
    runner_port: u16,
    /// How many runners serve each account.
    #[arg(long, env = "SELVEDGE_REPLICAS", default_value_t = 2)]
    replicas: usize,
    /// Trust X-Forwarded-For/-Proto from the load balancer in front.
    #[arg(long, env = "SELVEDGE_TRUST_FORWARDED")]
    trust_forwarded: bool,
    /// Fill a request.cf field from a header the load balancer sets, e.g.
    /// country=x-geo-country. Repeatable; needs --trust-forwarded.
    #[arg(long = "cf-header", env = "SELVEDGE_CF_HEADERS", value_delimiter = ',')]
    cf_headers: Vec<String>,
    /// request.cf.colo: a name for this site.
    #[arg(long, env = "SELVEDGE_COLO")]
    colo: Option<String>,
}

#[derive(Args, Clone)]
struct AllArgs {
    #[command(flatten)]
    api: ApiArgs,
    #[arg(long, env = "SELVEDGE_DATA_LISTEN", default_value = "127.0.0.1:8788")]
    data_listen: SocketAddr,
    #[arg(long, env = "SELVEDGE_AGENT_LISTEN", default_value = "127.0.0.1:8789")]
    agent_listen: SocketAddr,
    /// Serve R2's S3-compatible API here (path-style).
    #[arg(long, env = "SELVEDGE_R2_S3_LISTEN")]
    r2_s3_listen: Option<SocketAddr>,
    #[arg(long, env = "SELVEDGE_GATEWAY_LISTEN", default_value = "0.0.0.0:8080")]
    gateway_listen: SocketAddr,
    #[arg(long, env = "SELVEDGE_WORKERD", default_value = "workerd")]
    workerd: PathBuf,
    #[arg(long, env = "SELVEDGE_STATE_DIR", default_value = "/var/lib/selvedge")]
    state_dir: PathBuf,
    #[arg(long, env = "SELVEDGE_RUN_DIR", default_value = "/run/selvedge")]
    run_dir: PathBuf,
    #[arg(long, env = "SELVEDGE_DO_BUCKET")]
    do_bucket: Option<String>,
    #[arg(long = "do-bucket-option", env = "SELVEDGE_DO_BUCKET_OPTIONS", value_delimiter = ',')]
    do_bucket_options: Vec<String>,
    #[arg(long, env = "SELVEDGE_CLUSTER_SECRET", hide_env_values = true)]
    cluster_secret: Option<String>,
    /// Run each account's workerd as its own Linux user (see `agent --help`).
    #[arg(long, env = "SELVEDGE_ACCOUNT_UIDS")]
    account_uids: Option<String>,
    /// Memory limit per account process, in MiB (0: no limits).
    #[arg(long, env = "SELVEDGE_ACCOUNT_MEMORY_MB", default_value_t = 1024)]
    account_memory_mb: u64,
    /// CPUs an account process may use.
    #[arg(long, env = "SELVEDGE_ACCOUNT_CPUS", default_value_t = 1.0)]
    account_cpus: f64,
    /// Kill a hung account process after this many seconds (see `agent --help`).
    #[arg(long, env = "SELVEDGE_HANG_TIMEOUT", default_value_t = 30)]
    hang_timeout: u64,
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Create an account and print its id and an account API token.
    CreateAccount {
        #[command(flatten)]
        db: DbArgs,
        name: String,
    },
}

fn init_logging(json: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
    tracing::info!("shutting down");
}

async fn open_db(args: &DbArgs) -> Result<Db> {
    let db = Db::connect(&args.database_url, args.database_pool).await?;
    db.migrate().await?;
    Ok(db)
}

fn blob_options() -> Vec<(String, String)> {
    // object_store reads AWS_* itself; nothing extra by default.
    vec![]
}

async fn api_state(args: &ApiArgs) -> Result<Arc<ApiState>> {
    let db = open_db(&args.db).await?;
    if let Some(t) = &args.admin_token {
        db.ensure_admin_token(t).await?;
    }
    Ok(Arc::new(ApiState {
        tails: selvedge_server::tail::Hub::start(db.clone()),
        db,
        blobs: Blobs::open(&args.blob_url, &blob_options())?,
        secrets: SecretBox::from_hex(&args.master_key)?,
        internal_token: args.internal.internal_token.clone(),
        workers_dev_zone: args.workers_dev_zone.clone(),
        r2_dev_zone: args.r2_dev_zone.clone(),
        gateway_url: args.gateway_url.as_ref().map(|u| u.trim_end_matches('/').to_owned()),
        http: selvedge_server::tls::reqwest(
            reqwest::Client::builder(),
            &[selvedge_server::tls::Role::Gateway],
        )
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(60))
        .build()?,
    }))
}

async fn serve_axum(
    listen: SocketAddr,
    app: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener =
        tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
    tracing::info!(addr = %listen, "listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

/// Serves an internal-only listener: with mTLS, every client needs a
/// platform certificate of one of `clients`.
async fn serve_axum_internal(
    listen: SocketAddr,
    clients: &'static [selvedge_server::tls::Role],
    app: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    if selvedge_server::tls::get().is_none() {
        return serve_axum(listen, app, shutdown).await;
    }
    let listener =
        tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
    tracing::info!(addr = %listen, "listening (mTLS)");
    let app = app
        .layer(axum::middleware::from_fn(selvedge_server::tls::health_gate))
        .into_make_service_with_connect_info::<selvedge_server::tls::Peer>();
    axum::serve(selvedge_server::tls::Listener::new(listener, clients)?, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// With mTLS, the API's public listener refuses the internal endpoints.
async fn refuse_internal(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req.uri().path().starts_with("/internal/") {
        return axum::response::IntoResponse::into_response((axum::http::StatusCode::NOT_FOUND, "not found"));
    }
    next.run(req).await
}

fn agent_config(a: &AgentArgs) -> Result<AgentConfig> {
    let cluster = match (&a.do_bucket, &a.cluster_secret) {
        (Some(bucket), Some(secret)) => Some(AgentCluster {
            bucket: bucket.clone(),
            bucket_options: a.do_bucket_options.clone(),
            bind_host: a.cluster_host.clone(),
            secret: secret.clone(),
        }),
        (Some(_), None) => anyhow::bail!("--do-bucket needs --cluster-secret"),
        _ => None,
    };
    Ok(AgentConfig {
        listen: a.listen,
        api_url: a.api_url.trim_end_matches('/').to_owned(),
        data_addr: a.data_addr.clone(),
        internal_token: a.internal.internal_token.clone(),
        workerd: a.workerd.clone(),
        state_dir: a.state_dir.clone(),
        run_dir: a.run_dir.clone(),
        idle_timeout: Duration::from_secs(a.idle_timeout),
        cluster,
        allow_local_durable_objects: a.allow_local_durable_objects,
        drain_delay: Duration::from_secs(a.drain_delay),
        asset_cache_bytes: a.asset_cache_mb << 20,
        account_uids: a.account_uids.as_deref().map(selvedge_server::agent::uids::parse_range).transpose()?,
        limits: (a.account_memory_mb > 0).then_some(selvedge_server::agent::limits::Limits {
            memory_bytes: a.account_memory_mb << 20,
            cpus: a.account_cpus,
            pids: a.account_pids,
        }),
        hang_timeout: (a.hang_timeout > 0).then(|| Duration::from_secs(a.hang_timeout)),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_json);
    selvedge_server::tls::init_from_env()?;
    if matches!(cli.command, Command::All(_)) && selvedge_server::tls::get().is_some() {
        anyhow::bail!(
            "`selvedge all` runs every component in one process; mTLS (SELVEDGE_TLS_*) is for separate ones"
        );
    }
    if let Some(addr) = cli.metrics_listen {
        tokio::spawn(async move {
            if let Err(e) = selvedge_server::metrics::serve(addr).await {
                tracing::error!(error = %format!("{e:#}"), "metrics listener failed");
            }
        });
    }
    match cli.command {
        Command::Migrate(db) => {
            open_db(&db).await?;
            tracing::info!("migrations applied");
        }
        Command::Admin(AdminCommand::CreateAccount { db, name }) => {
            let db = open_db(&db).await?;
            let account = db.create_account(&name).await?;
            let (token_id, token) = db.create_token(Some(&account.id), "initial").await?;
            println!(
                "account_id={}\napi_token={token}\nr2_access_key_id={token_id}\nr2_secret_access_key={}",
                account.id,
                selvedge_core::sha256_hex(token.as_bytes())
            );
        }
        Command::Api(args) => {
            let state = api_state(&args).await?;
            let router = selvedge_server::api::router(state);
            if selvedge_server::tls::get().is_some() {
                let addr = args
                    .internal_listen
                    .context("mTLS needs --internal-listen for the API's internal endpoints")?;
                let internal = router.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_axum_internal(
                        addr,
                        selvedge_server::tls::API_INTERNAL_CLIENTS,
                        internal,
                        shutdown_signal(),
                    )
                    .await
                    {
                        tracing::error!(error = %format!("{e:#}"), "API internal listener failed");
                    }
                });
                // The public listener: everything but the internal endpoints.
                let public = router.layer(axum::middleware::from_fn(refuse_internal));
                serve_axum(args.listen, public, shutdown_signal()).await?;
            } else {
                serve_axum(args.listen, router, shutdown_signal()).await?;
            }
        }
        Command::Data(args) => {
            let db = open_db(&args.db).await?;
            let blobs = Blobs::open(&args.blob_url, &blob_options())?;
            let r2_s3 = args.r2_s3_listen.map(|addr| {
                (
                    addr,
                    Arc::new(selvedge_server::r2s3::S3State {
                        db: db.clone(),
                        blobs: blobs.clone(),
                        r2_dev_zone: args.r2_dev_zone.clone(),
                        domain: args.r2_s3_domain.clone().filter(|d| !d.is_empty()),
                    }),
                )
            });
            let state = selvedge_server::data::DataState::new(
                db.clone(),
                blobs.clone(),
                args.internal.internal_token.clone(),
            );
            tokio::spawn(purge_expired(db.clone()));
            tokio::spawn(collect_garbage(db.clone(), blobs));
            match &args.gateway_url {
                Some(url) => {
                    let d = selvedge_server::dispatch::Dispatcher::new(
                        db.clone(),
                        url.clone(),
                        args.internal.internal_token.clone(),
                    );
                    tokio::spawn(d.run_cron());
                    let q = selvedge_server::queues::QueueDispatcher::new(
                        db,
                        url.clone(),
                        args.internal.internal_token.clone(),
                    );
                    tokio::spawn(q.run());
                }
                None => tracing::warn!(
                    "SELVEDGE_GATEWAY_URL is not set: Cron Triggers and queue consumers will not run"
                ),
            }
            if let Some((addr, s3)) = r2_s3 {
                tokio::spawn(async move {
                    if let Err(e) =
                        serve_axum(addr, selvedge_server::r2s3::router(s3), shutdown_signal()).await
                    {
                        tracing::error!(error = %format!("{e:#}"), "R2 S3 API listener failed");
                    }
                });
            }
            serve_axum_internal(
                args.listen,
                selvedge_server::tls::DATA_CLIENTS,
                selvedge_server::data::router(state),
                shutdown_signal(),
            )
            .await?;
        }
        Command::Agent(args) => {
            selvedge_server::agent::run(agent_config(&args)?, shutdown_signal()).await?;
        }
        Command::Gateway(args) => {
            let runners = match args.runner_dns {
                Some(host) => Runners::Dns { host, port: args.runner_port },
                None => Runners::Static(args.runners),
            };
            selvedge_server::gateway::run(
                GatewayConfig {
                    listen: args.listen,
                    internal_listen: args.internal_listen,
                    api_url: args.api_url.trim_end_matches('/').to_owned(),
                    internal_token: args.internal.internal_token,
                    runners,
                    replicas: args.replicas,
                    trust_forwarded: args.trust_forwarded,
                    cf_headers: {
                        let h = selvedge_server::gateway::parse_cf_headers(&args.cf_headers)?;
                        anyhow::ensure!(
                            h.is_empty() || args.trust_forwarded,
                            "--cf-header reads headers the load balancer sets, so it needs --trust-forwarded"
                        );
                        h
                    },
                    colo: args.colo.filter(|c| !c.is_empty()),
                },
                shutdown_signal(),
            )
            .await?;
        }
        Command::All(args) => run_all(*args).await?,
    }
    Ok(())
}

async fn purge_expired(db: Db) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        match db.kv_purge_expired().await {
            Ok(0) => {}
            Ok(n) => tracing::debug!(n, "purged expired KV entries"),
            Err(e) => tracing::warn!(error = %e, "purging expired KV entries failed"),
        }
        match db.purge_retained_messages().await {
            Ok(0) => {}
            Ok(n) => tracing::debug!(n, "dropped queue messages past their retention period"),
            Err(e) => tracing::warn!(error = %e, "dropping old queue messages failed"),
        }
    }
}

/// Deletes object store blobs that nothing references any more, ten minutes
/// after they were released, so reads already streaming them can finish.
async fn collect_garbage(db: Db, blobs: Blobs) {
    // Tests shorten these; operators have no reason to.
    let interval = std::env::var("SELVEDGE_GC_INTERVAL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        match selvedge_server::r2::collect_garbage(&db, &blobs, 600).await {
            Ok(0) => {}
            Ok(n) => tracing::debug!(n, "deleted unreferenced blobs"),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "deleting unreferenced blobs failed"),
        }
        // Static asset files no version uses, a day after their last use.
        let grace =
            std::env::var("SELVEDGE_ASSET_GC_GRACE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(86_400);
        match db.collect_asset_files(grace, 1000).await {
            Ok(paths) => {
                for p in &paths {
                    if let Err(e) = blobs.delete(p).await {
                        tracing::warn!(error = %format!("{e:#}"), path = %p, "deleting an asset blob failed");
                    }
                }
                if !paths.is_empty() {
                    tracing::info!(n = paths.len(), "deleted unused static asset files");
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "collecting static asset files failed"),
        }
        // R2 lifecycle rules: expired objects and stale multipart uploads.
        match selvedge_server::r2lifecycle::apply(&db).await {
            Ok((0, 0)) => {}
            Ok((deleted, aborted)) => tracing::info!(deleted, aborted, "applied R2 lifecycle rules"),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "applying R2 lifecycle rules failed"),
        }
        // D1 import and export files whose URLs expired unused.
        match db.take_expired_d1_transfers().await {
            Ok(paths) => {
                for p in &paths {
                    if let Err(e) = blobs.delete(p).await {
                        tracing::warn!(error = %format!("{e:#}"), path = %p, "deleting a D1 transfer file failed");
                    }
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "collecting D1 transfer files failed"),
        }
    }
}

/// Every role in one process, for a single node.
async fn run_all(mut args: AllArgs) -> Result<()> {
    if args.api.gateway_url.is_none() {
        args.api.gateway_url = Some(format!("http://{}", loopback(args.gateway_listen)));
    }
    let state = api_state(&args.api).await?;
    let token = args.api.internal.internal_token.clone();
    let (tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let stop = |tx: &tokio::sync::broadcast::Sender<()>| {
        let mut rx = tx.subscribe();
        async move {
            let _ = rx.recv().await;
        }
    };
    let api_url = format!("http://{}", loopback(args.api.listen));
    let data = selvedge_server::data::DataState::new(state.db.clone(), state.blobs.clone(), token.clone());
    let s3_state = Arc::new(selvedge_server::r2s3::S3State {
        db: state.db.clone(),
        blobs: state.blobs.clone(),
        r2_dev_zone: args.api.r2_dev_zone.clone(),
        domain: std::env::var("SELVEDGE_R2_S3_DOMAIN").ok().filter(|d| !d.is_empty()),
    });
    tokio::spawn(purge_expired(state.db.clone()));
    tokio::spawn(collect_garbage(state.db.clone(), state.blobs.clone()));
    let dispatcher = selvedge_server::dispatch::Dispatcher::new(
        state.db.clone(),
        args.api.gateway_url.clone().expect("set above"),
        token.clone(),
    );
    tokio::spawn(dispatcher.run_cron());
    let queues = selvedge_server::queues::QueueDispatcher::new(
        state.db.clone(),
        args.api.gateway_url.clone().expect("set above"),
        token.clone(),
    );
    tokio::spawn(queues.run());
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(serve_axum(args.api.listen, selvedge_server::api::router(state), stop(&tx)));
    tasks.spawn(serve_axum(args.data_listen, selvedge_server::data::router(data), stop(&tx)));
    if let Some(addr) = args.r2_s3_listen {
        tasks.spawn(serve_axum(addr, selvedge_server::r2s3::router(s3_state), stop(&tx)));
    }
    let agent = agent_config(&AgentArgs {
        internal: args.api.internal.clone(),
        listen: args.agent_listen,
        api_url: api_url.clone(),
        data_addr: loopback(args.data_listen).to_string(),
        workerd: args.workerd,
        state_dir: args.state_dir,
        run_dir: args.run_dir,
        idle_timeout: 600,
        do_bucket: args.do_bucket,
        do_bucket_options: args.do_bucket_options,
        cluster_host: "127.0.0.1".into(),
        cluster_secret: args.cluster_secret,
        allow_local_durable_objects: true,
        drain_delay: 0,
        asset_cache_mb: 1024,
        account_uids: args.account_uids,
        account_memory_mb: args.account_memory_mb,
        account_cpus: args.account_cpus,
        account_pids: 1024,
        hang_timeout: args.hang_timeout,
    })?;
    tasks.spawn(selvedge_server::agent::run(agent, stop(&tx)));
    tasks.spawn(selvedge_server::gateway::run(
        GatewayConfig {
            listen: args.gateway_listen,
            internal_listen: None,
            api_url,
            internal_token: token,
            runners: Runners::Static(vec![loopback(args.agent_listen).to_string()]),
            replicas: 1,
            trust_forwarded: false,
            cf_headers: vec![],
            colo: None,
        },
        stop(&tx),
    ));
    tokio::select! {
        () = shutdown_signal() => {}
        Some(r) = tasks.join_next() => {
            r??;
            anyhow::bail!("a role exited unexpectedly");
        }
    }
    let _ = tx.send(());
    while let Some(r) = tasks.join_next().await {
        r??;
    }
    Ok(())
}

/// A wildcard listen address, as a loopback address to connect to.
fn loopback(addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() { SocketAddr::from(([127, 0, 0, 1], addr.port())) } else { addr }
}
