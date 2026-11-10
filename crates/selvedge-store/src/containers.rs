//! Container applications (RFC 0002).

use anyhow::Result;
use selvedge_core::AccountId;
use selvedge_core::containers::Resources;
use serde_json::Value;

use crate::Db;

#[derive(Debug, Clone)]
pub struct ContainerApp {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub namespace_id: String,
    pub script_name: String,
    pub class_name: String,
    pub image: String,
    pub max_instances: i32,
    pub resources: Resources,
    /// The application as the client sent it.
    pub spec: Value,
    pub version: i32,
    pub created_at: String,
}

/// What a new or changed application is stored with.
#[derive(Debug, Clone)]
pub struct ContainerAppFields {
    pub name: String,
    pub namespace_id: String,
    pub script_name: String,
    pub class_name: String,
    pub image: String,
    pub max_instances: i32,
    pub resources: Resources,
    pub spec: Value,
}

const COLUMNS: &str = "id, account_id, name, namespace_id, script_name, class_name, image, max_instances,
    vcpu, memory_mib, disk_mb, spec, version,
    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')";

fn row(r: &tokio_postgres::Row) -> ContainerApp {
    ContainerApp {
        id: r.get(0),
        account_id: r.get(1),
        name: r.get(2),
        namespace_id: r.get(3),
        script_name: r.get(4),
        class_name: r.get(5),
        image: r.get(6),
        max_instances: r.get(7),
        resources: Resources {
            vcpu: r.get(8),
            memory_mib: r.get::<_, i64>(9) as u64,
            disk_mb: r.get::<_, i64>(10) as u64,
        },
        spec: r.get(11),
        version: r.get(12),
        created_at: r.get(13),
    }
}

async fn bump(tx: &tokio_postgres::Transaction<'_>, account: &AccountId) -> Result<()> {
    tx.execute(
        "UPDATE accounts SET generation = nextval('account_generations') WHERE id = $1",
        &[&account.as_str()],
    )
    .await?;
    Ok(())
}

impl Db {
    /// Creates an application; `None` if the account already has one with
    /// this name or for this Durable Object namespace. Bumps the generation,
    /// so runners give the class its container.
    pub async fn create_container_app(
        &self,
        account: &AccountId,
        f: &ContainerAppFields,
    ) -> Result<Option<ContainerApp>> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let r = tx
            .query_opt(
                &format!(
                    "INSERT INTO container_applications
                       (id, account_id, name, namespace_id, script_name, class_name, image, max_instances,
                        vcpu, memory_mib, disk_mb, spec)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                     ON CONFLICT DO NOTHING RETURNING {COLUMNS}"
                ),
                &[
                    &selvedge_core::new_version_id(),
                    &account.as_str(),
                    &f.name,
                    &f.namespace_id,
                    &f.script_name,
                    &f.class_name,
                    &f.image,
                    &f.max_instances,
                    &f.resources.vcpu,
                    &(f.resources.memory_mib as i64),
                    &(f.resources.disk_mb as i64),
                    &f.spec,
                ],
            )
            .await?;
        if r.is_some() {
            bump(&tx, account).await?;
        }
        tx.commit().await?;
        Ok(r.as_ref().map(row))
    }

    pub async fn container_apps(&self, account: &AccountId) -> Result<Vec<ContainerApp>> {
        let rows = self
            .client()
            .await?
            .query(
                &format!("SELECT {COLUMNS} FROM container_applications WHERE account_id = $1 ORDER BY name"),
                &[&account.as_str()],
            )
            .await?;
        Ok(rows.iter().map(row).collect())
    }

    pub async fn container_app(&self, account: &AccountId, id: &str) -> Result<Option<ContainerApp>> {
        let r = self
            .client()
            .await?
            .query_opt(
                &format!("SELECT {COLUMNS} FROM container_applications WHERE account_id = $1 AND id = $2"),
                &[&account.as_str(), &id],
            )
            .await?;
        Ok(r.as_ref().map(row))
    }

    /// The application of a Durable Object class, as the containers
    /// controller looks it up to start an instance.
    pub async fn container_app_of_class(
        &self,
        account: &AccountId,
        script: &str,
        class: &str,
    ) -> Result<Option<ContainerApp>> {
        let r = self
            .client()
            .await?
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM container_applications
                     WHERE account_id = $1 AND script_name = $2 AND class_name = $3"
                ),
                &[&account.as_str(), &script, &class],
            )
            .await?;
        Ok(r.as_ref().map(row))
    }

    /// Replaces what an application runs; `None` if it does not exist. The
    /// name and the Durable Object namespace of `f` are ignored: they do not
    /// change.
    pub async fn update_container_app(
        &self,
        account: &AccountId,
        id: &str,
        f: &ContainerAppFields,
    ) -> Result<Option<ContainerApp>> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let r = tx
            .query_opt(
                &format!(
                    "UPDATE container_applications
                     SET image = $3, max_instances = $4, vcpu = $5, memory_mib = $6, disk_mb = $7, spec = $8,
                         version = version + 1, updated_at = now()
                     WHERE account_id = $1 AND id = $2 RETURNING {COLUMNS}"
                ),
                &[
                    &account.as_str(),
                    &id,
                    &f.image,
                    &f.max_instances,
                    &f.resources.vcpu,
                    &(f.resources.memory_mib as i64),
                    &(f.resources.disk_mb as i64),
                    &f.spec,
                ],
            )
            .await?;
        if r.is_some() {
            bump(&tx, account).await?;
        }
        tx.commit().await?;
        Ok(r.as_ref().map(row))
    }

    pub async fn delete_container_app(&self, account: &AccountId, id: &str) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let n = tx
            .execute(
                "DELETE FROM container_applications WHERE account_id = $1 AND id = $2",
                &[&account.as_str(), &id],
            )
            .await?;
        if n > 0 {
            bump(&tx, account).await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }

    /// Records a rollout and answers its id and creation time.
    pub async fn record_container_rollout(
        &self,
        application: &str,
        spec: &Value,
    ) -> Result<(String, String)> {
        let id = selvedge_core::new_version_id();
        let r = self
            .client()
            .await?
            .query_one(
                "INSERT INTO container_rollouts (id, application_id, spec) VALUES ($1, $2, $3)
                 RETURNING to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')",
                &[&id, &application, spec],
            )
            .await?;
        Ok((id, r.get(0)))
    }

    /// Runs `f` while holding a lock that serializes container starts for an
    /// account across controller replicas: the controller counts instances
    /// and creates one under it, so two starts cannot both pass a limit.
    pub async fn with_container_start_lock<T>(
        &self,
        account: &AccountId,
        f: impl std::future::Future<Output = T>,
    ) -> Result<T> {
        // A session lock on a pooled connection: if this future is dropped
        // midway, the guard closes the connection, which releases the lock.
        struct Guard(Option<deadpool_postgres::Object>);
        impl Drop for Guard {
            fn drop(&mut self) {
                if let Some(c) = self.0.take() {
                    drop(deadpool_postgres::Object::take(c));
                }
            }
        }
        let key = START_LOCK
            ^ i64::from_str_radix(&selvedge_core::sha256_hex(account.as_str().as_bytes())[..15], 16)?;
        let mut guard = Guard(Some(self.client().await?));
        let client = guard.0.as_ref().expect("set above");
        client.execute("SELECT pg_advisory_lock($1)", &[&key]).await?;
        let out = f.await;
        if client.execute("SELECT pg_advisory_unlock($1)", &[&key]).await.is_ok() {
            // Unlocked: the connection may go back to the pool.
            drop(guard.0.take());
        }
        Ok(out)
    }
}

/// Distinguishes container start locks from other advisory locks.
const START_LOCK: i64 = 0x5e1_7c0d_e000_0000;
