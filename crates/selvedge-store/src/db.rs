//! PostgreSQL connection pool and append-only migrations.

use anyhow::{Context, Result};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::NoTls;

/// Migrations in order. Released entries are never edited or removed.
const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
    (2, include_str!("../migrations/0002_r2.sql")),
    (3, include_str!("../migrations/0003_generation_sequence.sql")),
];

/// An arbitrary constant identifying Selvedge's migration lock.
const MIGRATION_LOCK: i64 = 0x5e1_7ed9e;

#[derive(Clone)]
pub struct Db {
    pool: Pool,
}

impl Db {
    pub async fn connect(url: &str, max_connections: usize) -> Result<Self> {
        let config: tokio_postgres::Config = url.parse().context("parsing database URL")?;
        let manager =
            Manager::from_config(config, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
        let pool = Pool::builder(manager).max_size(max_connections).build()?;
        // Fail fast on a bad URL or an unreachable server.
        drop(pool.get().await.context("connecting to PostgreSQL")?);
        Ok(Self { pool })
    }

    pub async fn client(&self) -> Result<deadpool_postgres::Object> {
        Ok(self.pool.get().await?)
    }

    /// Applies pending migrations. Safe to call from several replicas at once:
    /// an advisory lock serializes them.
    pub async fn migrate(&self) -> Result<()> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK]).await?;
        tx.batch_execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version integer PRIMARY KEY,
                applied_at timestamptz NOT NULL DEFAULT now()
            )",
        )
        .await?;
        let applied: Option<i32> =
            tx.query_one("SELECT max(version) FROM schema_migrations", &[]).await?.get(0);
        let applied = applied.unwrap_or(0);
        let newest = MIGRATIONS.last().map(|(v, _)| *v).unwrap_or(0);
        anyhow::ensure!(
            applied <= newest,
            "database schema version {applied} is newer than this binary supports ({newest}); \
             downgrades are not supported"
        );
        for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > applied) {
            tracing::info!(version, "applying migration");
            tx.batch_execute(sql).await.with_context(|| format!("migration {version}"))?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES ($1)", &[version]).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
