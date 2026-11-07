//! Public access to R2 buckets: the r2.dev-style URL and custom domains.

use anyhow::Result;
use selvedge_core::HexId;
use tokio_postgres::error::SqlState;

use crate::Db;

/// A bucket's r2.dev-style URL: `pub-<id>.<zone>`.
#[derive(Debug, Clone)]
pub struct DevUrl {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct CustomDomain {
    pub domain: String,
    pub zone_id: String,
    pub zone_name: String,
    pub enabled: bool,
    pub min_tls: String,
}

fn custom(r: &tokio_postgres::Row) -> CustomDomain {
    CustomDomain {
        domain: r.get(0),
        zone_id: r.get(1),
        zone_name: r.get(2),
        enabled: r.get(3),
        min_tls: r.get(4),
    }
}

const CUSTOM_COLUMNS: &str = "c.domain, c.zone_id, z.name, c.enabled, c.min_tls";

impl Db {
    /// The bucket's dev URL; its id is chosen on first use and kept.
    pub async fn r2_dev_url(&self, bucket: &str) -> Result<DevUrl> {
        self.set_r2_dev_url(bucket, None).await
    }

    /// Turns the dev URL on or off (`None`: leaves it as it is).
    pub async fn set_r2_dev_url(&self, bucket: &str, enabled: Option<bool>) -> Result<DevUrl> {
        let row = self
            .client()
            .await?
            .query_one(
                "UPDATE r2_buckets SET dev_url_id = COALESCE(dev_url_id, $2),
                   dev_url_enabled = COALESCE($3, dev_url_enabled)
                 WHERE id = $1 RETURNING dev_url_id, dev_url_enabled",
                &[&bucket, &HexId::random().as_str(), &enabled],
            )
            .await?;
        Ok(DevUrl { id: row.get(0), enabled: row.get(1) })
    }

    pub async fn r2_custom_domains(&self, bucket: &str) -> Result<Vec<CustomDomain>> {
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT {CUSTOM_COLUMNS} FROM r2_custom_domains c JOIN zones z ON z.id = c.zone_id
                     WHERE c.bucket_id = $1 ORDER BY c.domain"
                ),
                &[&bucket],
            )
            .await?;
        Ok(rows.iter().map(custom).collect())
    }

    pub async fn r2_custom_domain(&self, bucket: &str, domain: &str) -> Result<Option<CustomDomain>> {
        let row = self
            .client()
            .await?
            .query_opt(
                &format!(
                    "SELECT {CUSTOM_COLUMNS} FROM r2_custom_domains c JOIN zones z ON z.id = c.zone_id
                     WHERE c.bucket_id = $1 AND c.domain = $2"
                ),
                &[&bucket, &domain],
            )
            .await?;
        Ok(row.as_ref().map(custom))
    }

    /// Connects a custom domain; `false` if the domain is taken (by any
    /// bucket, or by a Worker custom domain).
    pub async fn add_r2_custom_domain(
        &self,
        bucket: &str,
        domain: &str,
        zone: &str,
        enabled: bool,
        min_tls: &str,
    ) -> Result<bool> {
        let client = self.client().await?;
        let worker = client.query_opt("SELECT 1 FROM worker_domains WHERE hostname = $1", &[&domain]).await?;
        if worker.is_some() {
            return Ok(false);
        }
        let r = client
            .execute(
                "INSERT INTO r2_custom_domains (domain, bucket_id, zone_id, enabled, min_tls)
                 VALUES ($1, $2, $3, $4, $5)",
                &[&domain, &bucket, &zone, &enabled, &min_tls],
            )
            .await;
        match r {
            Ok(_) => Ok(true),
            Err(e) if e.code() == Some(&SqlState::UNIQUE_VIOLATION) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn update_r2_custom_domain(
        &self,
        bucket: &str,
        domain: &str,
        enabled: Option<bool>,
        min_tls: Option<&str>,
    ) -> Result<Option<CustomDomain>> {
        let n = self
            .client()
            .await?
            .execute(
                "UPDATE r2_custom_domains SET enabled = COALESCE($3, enabled), min_tls = COALESCE($4, min_tls)
                 WHERE bucket_id = $1 AND domain = $2",
                &[&bucket, &domain, &enabled, &min_tls],
            )
            .await?;
        if n == 0 {
            return Ok(None);
        }
        self.r2_custom_domain(bucket, domain).await
    }

    pub async fn delete_r2_custom_domain(&self, bucket: &str, domain: &str) -> Result<bool> {
        let n = self
            .client()
            .await?
            .execute(
                "DELETE FROM r2_custom_domains WHERE bucket_id = $1 AND domain = $2",
                &[&bucket, &domain],
            )
            .await?;
        Ok(n > 0)
    }

    /// The bucket a public hostname serves: `pub-<id>.<dev_zone>` with the dev
    /// URL on, or an enabled custom domain.
    pub async fn r2_public_bucket(&self, host: &str, dev_zone: Option<&str>) -> Result<Option<String>> {
        let client = self.client().await?;
        let dev_id = dev_zone
            .and_then(|z| host.strip_suffix(z))
            .and_then(|h| h.strip_suffix('.'))
            .and_then(|h| h.strip_prefix("pub-"))
            .filter(|id| !id.contains('.'));
        if let Some(id) = dev_id {
            let row = client
                .query_opt("SELECT id FROM r2_buckets WHERE dev_url_id = $1 AND dev_url_enabled", &[&id])
                .await?;
            return Ok(row.map(|r| r.get(0)));
        }
        let row = client
            .query_opt("SELECT bucket_id FROM r2_custom_domains WHERE domain = $1 AND enabled", &[&host])
            .await?;
        Ok(row.map(|r| r.get(0)))
    }
}
